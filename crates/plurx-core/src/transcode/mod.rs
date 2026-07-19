//! Transcode pipeline: pick an encoder, build the ffmpeg argument graph.
//!
//! The verdict of whether to transcode comes from [`crate::playback`]; this
//! module turns "transcode this file for that profile" into a concrete ffmpeg
//! invocation that produces HLS. Hardware *encode* is the big CPU win and is
//! selected per detected capability (NVENC/QSV/VAAPI/VideoToolbox), with a
//! software x264 fallback; HDR→SDR tone-mapping and subtitle burn-in run as
//! filters (ARCHITECTURE §3). Software and the hardware-encode-with-software-
//! filters paths are the Phase 2 targets; zero-copy GPU filter graphs are a
//! later refinement.

mod encoder;

pub use encoder::{detect_encoders, Encoder, EncoderCaps};

use crate::domain::MediaFile;

/// Segment length for on-the-fly HLS, in seconds. Keyframes are forced to
/// align to this so segments are independently decodable.
pub const SEGMENT_SECONDS: u32 = 4;

/// How HDR→SDR tone-mapping is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMap {
    /// CPU zscale+tonemap — always available, no GPU needed (the default).
    Zscale,
    /// libplacebo (Vulkan) — higher quality on a capable GPU; opt-in.
    Libplacebo,
}

/// What to burn into the video (image subs must be burned; text subs can be).
#[derive(Debug, Clone)]
pub struct SubtitleBurn {
    /// 0-based index among the file's subtitle streams.
    pub subtitle_index: i64,
    /// Whether the sub is a bitmap format (PGS/VobSub) → overlay, vs text.
    pub bitmap: bool,
}

/// Everything needed to build a transcode command.
#[derive(Debug, Clone)]
pub struct TranscodeOptions {
    pub target_height: i64,
    pub video_bitrate_kbps: u32,
    /// Audio: output channel count (2 = stereo downmix) and bitrate.
    pub audio_channels: u32,
    pub audio_bitrate_kbps: u32,
    /// 0-based index among the file's audio streams (default track otherwise).
    pub audio_index: Option<i64>,
    /// Start offset in seconds (resume / session start).
    pub start_seconds: f64,
    pub tone_map: ToneMap,
    pub subtitle_burn: Option<SubtitleBurn>,
}

impl Default for TranscodeOptions {
    fn default() -> Self {
        TranscodeOptions {
            target_height: 1080,
            video_bitrate_kbps: 8000,
            audio_channels: 2,
            audio_bitrate_kbps: 160,
            audio_index: None,
            start_seconds: 0.0,
            tone_map: ToneMap::Zscale,
            subtitle_burn: None,
        }
    }
}

/// Build the video filter chain: scale (never upscale) → tone-map (if the
/// source is HDR) → subtitle burn-in. Returns `None` when no filtering is
/// needed (rare for transcode, but keeps the caller simple).
fn video_filters(source: &MediaFile, opts: &TranscodeOptions, source_path: &str) -> String {
    let mut chain: Vec<String> = Vec::new();

    // Downscale to target height, keep aspect, even dims, never upscale.
    chain.push(format!("scale=-2:'min({h},ih)'", h = opts.target_height));

    // HDR → SDR tone-map when the source carries HDR.
    if source.hdr.is_some() {
        match opts.tone_map {
            ToneMap::Libplacebo => chain.push(
                "libplacebo=tonemapping=bt.2390:colorspace=bt709:color_primaries=bt709:\
                 color_trc=bt709:format=yuv420p"
                    .to_owned(),
            ),
            ToneMap::Zscale => chain.push(
                "zscale=t=linear:npl=100,format=gbrpf32le,\
                 tonemap=tonemap=hable:desat=0,\
                 zscale=p=bt709:t=bt709:m=bt709:r=tv,format=yuv420p"
                    .to_owned(),
            ),
        }
    } else {
        // Normalize to a browser-safe pixel format.
        chain.push("format=yuv420p".to_owned());
    }

    // Subtitle burn-in, last, so subs render at output resolution in the
    // output color space. Text/ASS is rendered with libass (covers the styled
    // anime-subtitle case, REQ-SUB-2). Bitmap subs (PGS/VobSub) need an
    // overlay filtergraph and are a documented fast-follow — requesting a
    // bitmap burn here simply skips it rather than producing a broken graph.
    if let Some(burn) = &opts.subtitle_burn {
        if !burn.bitmap {
            let escaped = escape_filter_path(source_path);
            chain.push(format!(
                "subtitles='{escaped}':si={idx}",
                idx = burn.subtitle_index
            ));
        }
    }

    chain.join(",")
}

/// Escape a path for use inside an ffmpeg filter argument (colons, quotes,
/// backslashes, commas are special).
fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

/// Build the full ffmpeg argument vector to transcode `source` into HLS in
/// `out_dir` (which must exist). Produces `index.m3u8` + `seg%05d.ts`.
pub fn hls_args(
    source: &MediaFile,
    encoder: Encoder,
    opts: &TranscodeOptions,
    out_dir: &str,
) -> Vec<String> {
    let source_path = source.path.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];

    // Hardware device init (VAAPI/QSV) must precede the input.
    args.extend(encoder.init_args());

    // Fast input seek for resume/session start.
    if opts.start_seconds > 0.0 {
        args.push("-ss".into());
        args.push(format!("{:.3}", opts.start_seconds));
    }

    // Hardware-accelerated decode where the encoder family implies it.
    args.extend(encoder.decode_args());

    args.push("-i".into());
    args.push(source_path.clone());

    // Map first video + chosen (or default) audio.
    args.push("-map".into());
    args.push("0:v:0".into());
    args.push("-map".into());
    match opts.audio_index {
        Some(i) => args.push(format!("0:a:{i}?")),
        None => args.push("0:a:0?".into()),
    }

    // Video filter chain (+ GPU upload suffix for VAAPI/QSV) + encoder.
    let mut vf = video_filters(source, opts, &source_path);
    if let Some(suffix) = encoder.filter_suffix() {
        vf.push(',');
        vf.push_str(suffix);
    }
    args.push("-vf".into());
    args.push(vf);
    args.extend(encoder.encode_args(opts.video_bitrate_kbps));

    // Segment-aligned keyframes so each segment is independently decodable.
    args.push("-force_key_frames".into());
    args.push(format!("expr:gte(t,n_forced*{SEGMENT_SECONDS})"));

    // Audio: downmix + AAC (browser-universal).
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-ac".into());
    args.push(opts.audio_channels.to_string());
    args.push("-b:a".into());
    args.push(format!("{}k", opts.audio_bitrate_kbps));

    // HLS muxer.
    args.extend(
        [
            "-f",
            "hls",
            "-hls_time",
            &SEGMENT_SECONDS.to_string(),
            "-hls_playlist_type",
            "event",
            "-hls_flags",
            "independent_segments+temp_file",
            "-hls_segment_type",
            "mpegts",
            "-hls_segment_filename",
            &format!("{out_dir}/seg%05d.ts"),
            "-start_number",
            "0",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args.push(format!("{out_dir}/index.m3u8"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MediaFile;

    fn file(hdr: Option<&str>) -> MediaFile {
        MediaFile {
            id: 1,
            item_id: 1,
            path: "/media/movie.mkv".into(),
            size: 1,
            mtime: 1,
            duration_ms: Some(600_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main 10".into()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            hdr: hdr.map(str::to_owned),
            bitrate: Some(60_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 1,
        }
    }

    #[test]
    fn software_hls_args_are_well_formed() {
        let opts = TranscodeOptions {
            target_height: 1080,
            video_bitrate_kbps: 6000,
            ..Default::default()
        };
        let args = hls_args(&file(None), Encoder::Software, &opts, "/tmp/sess");
        let joined = args.join(" ");
        assert!(joined.contains("-i /media/movie.mkv"));
        assert!(joined.contains("libx264"));
        assert!(joined.contains("scale=-2:'min(1080,ih)'"));
        assert!(joined.contains("-f hls"));
        assert!(joined.contains("/tmp/sess/index.m3u8"));
        assert!(joined.contains("expr:gte(t,n_forced*4)"));
        // SDR source → no tonemap, just pixel-format normalize.
        assert!(joined.contains("format=yuv420p"));
        assert!(!joined.contains("tonemap"));
    }

    #[test]
    fn hdr_source_inserts_tonemap() {
        let args = hls_args(
            &file(Some("hdr10")),
            Encoder::Software,
            &TranscodeOptions::default(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("tonemap=tonemap=hable"));
        assert!(joined.contains("zscale"));
    }

    #[test]
    fn start_offset_seeks_input() {
        let opts = TranscodeOptions {
            start_seconds: 90.5,
            ..Default::default()
        };
        let args = hls_args(&file(None), Encoder::Software, &opts, "/tmp/s");
        // -ss must come before -i for fast input seeking.
        let ss = args.iter().position(|a| a == "-ss").expect("has -ss");
        let i = args.iter().position(|a| a == "-i").expect("has -i");
        assert!(ss < i);
        assert_eq!(args[ss + 1], "90.500");
    }

    #[test]
    fn text_subtitle_burn_uses_libass() {
        let opts = TranscodeOptions {
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 2,
                bitmap: false,
            }),
            ..Default::default()
        };
        let args = hls_args(&file(None), Encoder::Software, &opts, "/tmp/s");
        assert!(args.join(" ").contains("subtitles='/media/movie.mkv':si=2"));
    }

    #[test]
    fn nvenc_swaps_encoder_and_adds_decode() {
        let args = hls_args(
            &file(None),
            Encoder::Nvenc,
            &TranscodeOptions::default(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("h264_nvenc"));
        assert!(joined.contains("cuda")); // hwaccel decode
    }
}
