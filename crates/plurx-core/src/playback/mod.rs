//! Playback decision engine.
//!
//! Pure function of (file media details, device profile) → a verdict of
//! direct play, remux, or transcode (ARCHITECTURE §3). Device profiles are
//! data (`profiles.toml`), so the direct-play matrix is correctable without a
//! release (REQ-PLAY-4). Phase 1 serves DirectPlay and Remux; a Transcode
//! verdict is reported honestly and its serving lands in Phase 2.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::domain::MediaFile;

/// Built-in device profiles, parsed once from the embedded TOML.
static PROFILES: LazyLock<HashMap<String, DeviceProfile>> = LazyLock::new(|| {
    let raw = include_str!("profiles.toml");
    toml::from_str::<HashMap<String, DeviceProfile>>(raw)
        .expect("embedded profiles.toml is valid")
        .into_iter()
        .map(|(name, mut p)| {
            p.name = name.clone();
            (name, p)
        })
        .collect()
});

/// Look up a built-in profile by name (e.g. `web-h264`).
pub fn profile(name: &str) -> Option<&'static DeviceProfile> {
    PROFILES.get(name)
}

/// The default profile when a client names none: the browser baseline.
pub fn default_profile() -> &'static DeviceProfile {
    profile("web-h264").expect("web-h264 profile exists")
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub containers: Vec<String>,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    #[serde(default)]
    pub max_height: Option<i64>,
    #[serde(default)]
    pub max_bitrate: Option<i64>,
    #[serde(default)]
    pub supports_hdr: bool,
}

impl DeviceProfile {
    fn allows_container(&self, container: &Option<String>) -> bool {
        match container {
            Some(c) => self.containers.iter().any(|x| x.eq_ignore_ascii_case(c)),
            None => false,
        }
    }
    fn allows_video(&self, codec: &Option<String>) -> bool {
        match codec {
            Some(c) => self.video_codecs.iter().any(|x| x.eq_ignore_ascii_case(c)),
            None => false,
        }
    }
    fn allows_audio(&self, codec: &str) -> bool {
        self.audio_codecs
            .iter()
            .any(|x| x.eq_ignore_ascii_case(codec))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackMethod {
    DirectPlay,
    Remux,
    Transcode,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub method: PlaybackMethod,
    /// Human-readable reasons the file isn't direct-playable (empty ⇒ direct).
    pub reasons: Vec<String>,
    /// For remux/transcode: re-encode audio to AAC because the source audio
    /// codec isn't in the profile.
    pub transcode_audio: bool,
    /// Target container for remux/transcode delivery.
    pub container: &'static str,
}

/// Decide how to play `file` on a device described by `profile`.
pub fn decide(file: &MediaFile, profile: &DeviceProfile) -> Decision {
    let mut reasons = Vec::new();

    let video_ok = file.video_codec.is_none() || profile.allows_video(&file.video_codec);
    if !video_ok {
        reasons.push(format!(
            "video codec {} unsupported",
            file.video_codec.as_deref().unwrap_or("unknown")
        ));
    }

    let height_ok = match (profile.max_height, file.height) {
        (Some(max), Some(h)) => h <= max,
        _ => true,
    };
    if !height_ok {
        reasons.push("resolution above device maximum".to_owned());
    }

    let bitrate_ok = match (profile.max_bitrate, file.bitrate) {
        (Some(max), Some(b)) => b <= max,
        _ => true,
    };
    if !bitrate_ok {
        reasons.push("bitrate above device maximum".to_owned());
    }

    let hdr_ok = file.hdr.is_none() || profile.supports_hdr;
    if !hdr_ok {
        reasons.push(format!(
            "HDR ({}) unsupported; tone-map required",
            file.hdr.as_deref().unwrap_or("hdr")
        ));
    }

    let container_ok = profile.allows_container(&file.container);
    if !container_ok {
        reasons.push(format!(
            "container {} unsupported",
            file.container.as_deref().unwrap_or("unknown")
        ));
    }

    // Audio is judged on the default track (else the first).
    let audio_codec = file
        .audio_streams
        .iter()
        .find(|a| a.default)
        .or_else(|| file.audio_streams.first())
        .map(|a| a.codec.clone());
    let audio_ok = match &audio_codec {
        Some(c) => profile.allows_audio(c),
        None => true, // no audio track — nothing to reject
    };
    if !audio_ok {
        reasons.push(format!(
            "audio codec {} unsupported",
            audio_codec.as_deref().unwrap_or("unknown")
        ));
    }

    // A manual A/V sync correction can only be applied by ffmpeg, so direct
    // play is off the table for that file — remux at minimum.
    let has_av_offset = file.audio_offset_ms != 0;
    if has_av_offset {
        reasons.push(format!("audio-sync correction {:+} ms", file.audio_offset_ms));
    }

    // Video/res/bitrate/HDR problems force a transcode; only a container or
    // audio mismatch (or the A/V offset) is remuxable (copy video, maybe
    // re-encode audio).
    let needs_transcode = !video_ok || !height_ok || !bitrate_ok || !hdr_ok;
    let method = if needs_transcode {
        PlaybackMethod::Transcode
    } else if !container_ok || !audio_ok || has_av_offset {
        PlaybackMethod::Remux
    } else {
        PlaybackMethod::DirectPlay
    };

    Decision {
        method,
        reasons,
        transcode_audio: !audio_ok,
        container: "mp4",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AudioStream, MediaFile};

    fn file(container: &str, vcodec: &str, acodec: &str) -> MediaFile {
        MediaFile {
            id: 1,
            item_id: 1,
            path: "/x".into(),
            size: 1,
            mtime: 1,
            duration_ms: Some(1000),
            container: Some(container.to_owned()),
            video_codec: Some(vcodec.to_owned()),
            video_profile: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(8_000_000),
            audio_streams: vec![AudioStream {
                index: 0,
                codec: acodec.to_owned(),
                channels: Some(2),
                default: true,
                ..Default::default()
            }],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
        }
    }

    #[test]
    fn profiles_load() {
        assert!(profile("web-h264").is_some());
        assert!(profile("directplay-any").is_some());
        assert_eq!(default_profile().name, "web-h264");
    }

    #[test]
    fn mp4_h264_aac_direct_plays_on_web() {
        let d = decide(&file("mp4", "h264", "aac"), default_profile());
        assert_eq!(d.method, PlaybackMethod::DirectPlay);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn mkv_h264_aac_remuxes_on_web() {
        // Right codecs, wrong container → remux, no audio transcode.
        let d = decide(&file("mkv", "h264", "aac"), default_profile());
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(!d.transcode_audio);
    }

    #[test]
    fn mkv_h264_ac3_remuxes_with_audio_transcode() {
        let d = decide(&file("mkv", "h264", "ac3"), default_profile());
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(
            d.transcode_audio,
            "ac3 not in web profile → re-encode audio"
        );
    }

    #[test]
    fn hevc_transcodes_on_web_but_direct_plays_on_native() {
        let hevc = file("mkv", "hevc", "aac");
        assert_eq!(
            decide(&hevc, default_profile()).method,
            PlaybackMethod::Transcode
        );
        let native = profile("directplay-any").expect("profile");
        assert_eq!(decide(&hevc, native).method, PlaybackMethod::DirectPlay);
    }

    #[test]
    fn hdr_forces_transcode_on_sdr_profile() {
        let mut f = file("mp4", "h264", "aac");
        f.hdr = Some("hdr10".to_owned());
        let d = decide(&f, default_profile());
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reasons.iter().any(|r| r.contains("HDR")));
    }
}
