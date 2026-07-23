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

/// Build an ad-hoc profile from a client's runtime-probed capabilities. The web
/// player detects what the *actual* browser can decode (`canPlayType` /
/// `MediaSource.isTypeSupported`) and reports it, so a file only transcodes when
/// this specific browser genuinely can't play it — this is the runtime-probe
/// refinement the fixed named profiles were always a placeholder for.
///
/// `supports_hdr` must fold in the display: HDR is only claimed when the screen
/// is HDR-capable, so HDR-on-SDR still tone-maps (grey/washed-out otherwise).
/// Resolution is intentionally *not* capped to the display — a decodable 4K
/// stream direct-plays and the browser downscales, per "direct play when it
/// works."
pub fn caps_profile(
    containers: Vec<String>,
    video_codecs: Vec<String>,
    audio_codecs: Vec<String>,
    max_height: Option<i64>,
    supports_hdr: bool,
) -> DeviceProfile {
    DeviceProfile {
        name: "client-caps".to_owned(),
        description: "runtime-probed browser capabilities".to_owned(),
        containers,
        video_codecs,
        audio_codecs,
        max_height,
        max_bitrate: None,
        supports_hdr,
    }
}

/// A manual override from the player's quality menu. `Auto` runs the normal
/// ladder; `Original` never transcodes video (direct/remux only — the caller's
/// error-fallback rescues an undecodable pick); `Transcode` forces a re-encode
/// at a client-chosen height (the height rides on the HLS start request, not
/// the verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force {
    Auto,
    Original,
    Transcode,
}

impl Force {
    pub fn parse(s: &str) -> Force {
        match s {
            "original" => Force::Original,
            "transcode" => Force::Transcode,
            _ => Force::Auto,
        }
    }
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

/// The per-dimension compatibility verdicts, shared by [`decide`] and
/// [`decide_forced`] so the two never drift.
struct Checks {
    video_ok: bool,
    height_ok: bool,
    bitrate_ok: bool,
    hdr_ok: bool,
    container_ok: bool,
    audio_ok: bool,
}

impl Checks {
    /// True when only the container and/or audio codec differ — copy-video
    /// remux territory (nothing needs a video re-encode).
    fn needs_transcode(&self) -> bool {
        !self.video_ok || !self.height_ok || !self.bitrate_ok || !self.hdr_ok
    }
}

/// Run every compatibility check, collecting human reasons for the ones that
/// fail (empty ⇒ direct-playable, profile-wise).
fn evaluate(file: &MediaFile, profile: &DeviceProfile) -> (Checks, Vec<String>) {
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
            "HDR ({}) needs tone-mapping for this display",
            file.hdr.as_deref().unwrap_or("hdr")
        ));
    }

    let container_ok = profile.allows_container(&file.container);
    if !container_ok {
        reasons.push(format!(
            "container {} not browser-native",
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

    (
        Checks {
            video_ok,
            height_ok,
            bitrate_ok,
            hdr_ok,
            container_ok,
            audio_ok,
        },
        reasons,
    )
}

/// Decide how to play `file` on a device described by `profile` — the automatic
/// ladder: direct play when everything matches, remux for a container/audio
/// mismatch (copy video, maybe re-encode audio), transcode only when the video
/// itself won't decode (codec/resolution/bitrate/HDR).
pub fn decide(file: &MediaFile, profile: &DeviceProfile) -> Decision {
    let (c, mut reasons) = evaluate(file, profile);

    // A manual A/V sync correction can only be applied by ffmpeg, so direct
    // play is off the table for that file — remux at minimum.
    let has_av_offset = file.audio_offset_ms != 0;
    if has_av_offset {
        reasons.push(format!(
            "audio-sync correction {:+} ms",
            file.audio_offset_ms
        ));
    }

    let method = if c.needs_transcode() {
        PlaybackMethod::Transcode
    } else if !c.container_ok || !c.audio_ok || has_av_offset {
        PlaybackMethod::Remux
    } else {
        PlaybackMethod::DirectPlay
    };

    Decision {
        method,
        reasons,
        transcode_audio: !c.audio_ok,
        container: "mp4",
    }
}

/// Like [`decide`], but honoring a manual quality override from the player.
pub fn decide_forced(file: &MediaFile, profile: &DeviceProfile, force: Force) -> Decision {
    match force {
        Force::Auto => decide(file, profile),
        Force::Transcode => {
            let (_, mut reasons) = evaluate(file, profile);
            reasons.insert(0, "forced transcode (manual quality)".to_owned());
            Decision {
                method: PlaybackMethod::Transcode,
                reasons,
                transcode_audio: true,
                container: "mp4",
            }
        }
        Force::Original => {
            // Never re-encode video: direct-play when the browser can take the
            // container + audio, else a copy-video remux. If the pick turns out
            // undecodable, the client's error path falls back to transcode.
            let (c, _) = evaluate(file, profile);
            let has_av_offset = file.audio_offset_ms != 0;
            let method = if c.container_ok && c.audio_ok && !has_av_offset {
                PlaybackMethod::DirectPlay
            } else {
                PlaybackMethod::Remux
            };
            Decision {
                method,
                reasons: vec!["forced original quality (no video transcode)".to_owned()],
                transcode_audio: !c.audio_ok,
                container: "mp4",
            }
        }
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

    // A browser that reports HEVC (e.g. Safari) turns a would-be transcode into
    // a copy-video remux — the whole point of runtime capability probing.
    #[test]
    fn hevc_direct_or_remuxes_when_browser_reports_it() {
        let hevc_mp4 = file("mp4", "hevc", "aac");
        let caps = caps_profile(
            vec!["mp4".into(), "webm".into()],
            vec!["h264".into(), "hevc".into()],
            vec!["aac".into(), "opus".into()],
            None,
            false,
        );
        assert_eq!(decide(&hevc_mp4, &caps).method, PlaybackMethod::DirectPlay);
        // Same codecs, MKV container → remux (copy video), not transcode.
        let hevc_mkv = file("mkv", "hevc", "aac");
        assert_eq!(decide(&hevc_mkv, &caps).method, PlaybackMethod::Remux);
    }

    #[test]
    fn caps_hdr_flag_gates_tone_mapping() {
        let mut f = file("mp4", "hevc", "aac");
        f.hdr = Some("hdr10".to_owned());
        let sdr = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            false,
        );
        assert_eq!(decide(&f, &sdr).method, PlaybackMethod::Transcode); // SDR display → tone-map
        let hdr = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
        );
        assert_eq!(decide(&f, &hdr).method, PlaybackMethod::DirectPlay); // HDR display → direct
    }

    #[test]
    fn four_k_direct_plays_when_uncapped() {
        let mut f = file("mp4", "h264", "aac");
        f.height = Some(2160);
        // No max_height in caps → a decodable 4K stream direct-plays (browser
        // downscales on a smaller screen).
        let caps = caps_profile(
            vec!["mp4".into()],
            vec!["h264".into()],
            vec!["aac".into()],
            None,
            false,
        );
        assert_eq!(decide(&f, &caps).method, PlaybackMethod::DirectPlay);
    }

    #[test]
    fn forced_original_never_transcodes_video() {
        // HEVC the browser can't take would auto-transcode; Original forces a
        // copy-video remux instead (client rescues if it truly won't decode).
        let hevc = file("mkv", "hevc", "aac");
        let d = decide_forced(&hevc, default_profile(), Force::Original);
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(decide(&hevc, default_profile()).method == PlaybackMethod::Transcode);
    }

    #[test]
    fn forced_transcode_overrides_a_direct_playable_file() {
        let mp4 = file("mp4", "h264", "aac");
        assert_eq!(
            decide(&mp4, default_profile()).method,
            PlaybackMethod::DirectPlay
        );
        let d = decide_forced(&mp4, default_profile(), Force::Transcode);
        assert_eq!(d.method, PlaybackMethod::Transcode);
    }

    #[test]
    fn forced_auto_matches_plain_decide() {
        let mkv = file("mkv", "h264", "ac3");
        assert_eq!(
            decide_forced(&mkv, default_profile(), Force::Auto).method,
            decide(&mkv, default_profile()).method
        );
    }
}
