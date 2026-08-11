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
    supports_dolby_vision: bool,
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
        supports_dolby_vision,
        dolby_vision_profiles: Vec::new(),
        remux_dolby_vision: false,
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
    /// Whether this client decodes a **Dolby Vision** stream as delivered.
    ///
    /// Separate from `supports_hdr`, and the distinction is the whole point:
    /// a DV track's base layer is often HDR10-compatible, so a client that
    /// reports HDR support looks able to take it — and then refuses, because
    /// what reaches its decoder is a track the container flags as Dolby
    /// Vision. Safari decodes those; Chrome does not, at any profile.
    /// Defaulted off, so a profile that has never heard of DV is assumed not
    /// to handle it.
    #[serde(default)]
    pub supports_dolby_vision: bool,
    /// Dolby Vision profile numbers this client has actually probed. New
    /// clients use this instead of the legacy all-or-nothing flag: Apple
    /// AVPlayer supports specific delivery profiles, not every disc profile.
    #[serde(default)]
    pub dolby_vision_profiles: Vec<u8>,
    /// This decoder accepts Dolby Vision through normalized HLS/fMP4 but not
    /// reliably as the source file over a progressive range URL. Apple
    /// AVPlayer can report a healthy P8 DV pipeline, advance the raw MP4's
    /// timeline, and still render only black frames. A copy remux keeps every
    /// video sample/RPU while rebuilding the delivery signaling it expects.
    #[serde(default)]
    pub remux_dolby_vision: bool,
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

    fn allows_dolby_vision(&self, file: &MediaFile) -> bool {
        self.supports_dolby_vision
            || dolby_vision_profile(file)
                .is_some_and(|profile| self.dolby_vision_profiles.contains(&profile))
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
    /// Preserve Dolby Vision configuration + RPU metadata on a copy/remux.
    /// False means the client cannot take this source profile and the remux
    /// must expose a compatible HDR base instead.
    pub preserve_dolby_vision: bool,
    /// Target container for remux/transcode delivery.
    pub container: &'static str,
    /// The dynamic range this plan actually delivers — see
    /// [`delivered_dynamic_range`]. Reported, not decided: it is a readout of
    /// the verdict above, so the play menu's badge can say "DV P7 → HDR10"
    /// instead of claiming the source's grade for a stripped remux. A session
    /// created later overrides it (MEDIA-BADGES-PLAN §3.2).
    pub delivered_dynamic_range: &'static str,
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

    let dolby_vision_needs_remux =
        profile.remux_dolby_vision && is_dolby_vision(file) && profile.allows_dolby_vision(file);
    let container_ok = profile.allows_container(&file.container) && !dolby_vision_needs_remux;
    if dolby_vision_needs_remux {
        reasons.push("Dolby Vision normalized through copy-video HLS for this device".to_owned());
    } else if !container_ok {
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

/// Is this source Dolby Vision — i.e. does it carry a DV configuration a
/// player either understands or chokes on?
pub fn is_dolby_vision(file: &MediaFile) -> bool {
    file.hdr.as_deref() == Some("dolby_vision")
}

/// The dynamic range of the bytes a delivery actually puts on the wire —
/// what the viewer is *getting*, as opposed to what the file carries.
///
/// A reporter, never a decider: it reads a verdict that has already been
/// made and names its dynamic-range outcome, so the badge in the play menu
/// can stop claiming the source's grade for a tone-mapped transcode of it
/// (MEDIA-BADGES-PLAN §2.1). The vocabulary is `MediaFile.hdr`'s plus
/// `"sdr"`, so a client compares source against delivered with string
/// equality.
///
/// The three deliveries are total:
///
/// - **Transcode** is always `"sdr"`. Every encoder is H.264
///   (`transcode/encoder.rs` — libx264 `-profile:v high`, h264_nvenc/qsv/
///   vaapi/videotoolbox, no 10-bit path) and every filter chain ends in
///   `yuv420p`/`nv12`; an HDR source goes through the tone-map graph. The
///   `ToneMap::None` escape hatch still lands on 8-bit `format=yuv420p`
///   (`transcode/mod.rs`), so "sdr" stays the honest answer — washed out at
///   worst, never a wider grade than claimed.
/// - **Direct play / remux of a non-DV source** delivers the source's grade:
///   the video is copied byte-for-byte.
/// - **Direct play / remux of a DV source** either preserves the Dolby
///   Vision configuration (`preserve_dolby_vision`) or strips it to the base
///   layer. Under [`decide`] a strip is only reachable through
///   `has_compatible_dv_base`, so the remux carries an "(HDR10-compatible)"
///   or "(HLG-compatible)" marker to read the base's grade off.
///
/// [`decide_forced`] with [`Force::Original`] is the one arm that reaches a
/// stripping remux outside that guarantee: it means "no video re-encode", so
/// it copies a DV source the client cannot decode whatever `dv_handling`
/// said. With no compatibility marker at all — a Profile 5 source, whose base
/// layer is not an HDR10 grade in any sense — this still answers `"hdr10"`,
/// and that over-claims. The over-claim is deliberately left rather than
/// guessed at: narrowing it needs `dv_strippable`, a fact about the server
/// rather than about the delivery this signature describes, and the stream in
/// question is one no client that asked for it can play — the error path
/// rescues it into a transcode, whose session then reports `"sdr"` for what
/// the viewer actually got.
pub fn delivered_dynamic_range(
    file: &MediaFile,
    method: PlaybackMethod,
    preserve_dolby_vision: bool,
) -> &'static str {
    if method == PlaybackMethod::Transcode {
        return "sdr";
    }
    match file.hdr.as_deref() {
        Some("dolby_vision") if preserve_dolby_vision => "dolby_vision",
        Some("dolby_vision") => {
            if file
                .hdr_format
                .as_deref()
                .is_some_and(|label| label.contains("HLG-compatible"))
            {
                "hlg"
            } else {
                "hdr10"
            }
        }
        Some("hdr10") => "hdr10",
        Some("hlg") => "hlg",
        // An SDR source, an HDR flavour nothing downstream distinguishes
        // (HDR10+ probes as "hdr10"), or a file nobody ever probed.
        _ => "sdr",
    }
}

/// Profile number from the scan's rich label (for example Profile 5 or 8).
/// Unknown is deliberately `None`: claiming every DV profile from a generic
/// HDR bit is the bug this profile-aware path replaces.
pub fn dolby_vision_profile(file: &MediaFile) -> Option<u8> {
    let label = file.hdr_format.as_deref()?;
    let after = label.to_ascii_lowercase();
    let after = after.split("profile").nth(1)?.trim_start();
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn has_compatible_dv_base(file: &MediaFile) -> bool {
    file.hdr_format
        .as_deref()
        .is_some_and(|label| label.contains("HDR10-compatible") || label.contains("HLG-compatible"))
}

/// What a Dolby Vision source needs doing about it for THIS client.
///
/// The failure this exists to prevent: a DV remux was handed to Chrome
/// because the base layer is HDR10-compatible and the client claimed HDR.
/// Chrome refuses the track outright (`MEDIA_ERR_DECODE`) — the DV
/// configuration is in the sample entry whatever the base layer looks like —
/// so the player's error path rescued it into a transcode at the Auto rung,
/// and a 4K disc remux played at 1080p with nothing saying why. Safari, which
/// decodes DV, played the same file perfectly: that difference is what named
/// the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DvHandling {
    /// Not DV, or the client decodes it: nothing to do.
    None,
    /// The client can't take DV, but the server can strip it to its HDR10
    /// base — which only ffmpeg can do, so direct play (the raw file) is out
    /// and a remux is the minimum.
    Strip,
    /// The client can't take DV and the server can't remove it. The only
    /// stream this client will play is a re-encoded one.
    Reencode,
}

fn dv_handling(file: &MediaFile, profile: &DeviceProfile, dv_strippable: bool) -> DvHandling {
    if !is_dolby_vision(file) || profile.allows_dolby_vision(file) {
        DvHandling::None
    } else if dv_strippable && has_compatible_dv_base(file) {
        DvHandling::Strip
    } else {
        DvHandling::Reencode
    }
}

/// Decide how to play `file` on a device described by `profile` — the automatic
/// ladder: direct play when everything matches, remux for a container/audio
/// mismatch (copy video, maybe re-encode audio), transcode only when the video
/// itself won't decode (codec/resolution/bitrate/HDR).
///
/// `dv_strippable` is a fact about the SERVER, not the device: whether this
/// ffmpeg build can remove a Dolby Vision configuration on the way out (the
/// `dovi_rpu` bitstream filter, ffmpeg 7.1+). It decides whether a DV source
/// a client cannot decode is a remux or a re-encode.
pub fn decide(file: &MediaFile, profile: &DeviceProfile, dv_strippable: bool) -> Decision {
    let (mut c, mut reasons) = evaluate(file, profile);
    let preserve_dolby_vision = is_dolby_vision(file) && profile.allows_dolby_vision(file);

    match dv_handling(file, profile, dv_strippable) {
        DvHandling::None => {}
        DvHandling::Strip => {
            // Not a transcode: the base layer is kept untouched and only the
            // DV configuration goes. But it takes ffmpeg, so the raw file
            // cannot be handed over as-is.
            c.container_ok = false;
            reasons.push(
                "Dolby Vision metadata removed for this device; compatible HDR base kept"
                    .to_owned(),
            );
        }
        DvHandling::Reencode => {
            c.video_ok = false;
            reasons.push(if has_compatible_dv_base(file) && !dv_strippable {
                "this Dolby Vision profile is unsupported by this device and this ffmpeg \
                 cannot expose its compatible HDR base (requires dovi_rpu in ffmpeg 7.1+)"
                    .to_owned()
            } else {
                "this Dolby Vision profile is unsupported by this device and has no \
                 compatible HDR base; transcoding"
                    .to_owned()
            });
        }
    }

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
        preserve_dolby_vision,
        container: "mp4",
        delivered_dynamic_range: delivered_dynamic_range(file, method, preserve_dolby_vision),
    }
}

/// Like [`decide`], but honoring a manual quality override from the player.
pub fn decide_forced(
    file: &MediaFile,
    profile: &DeviceProfile,
    force: Force,
    dv_strippable: bool,
) -> Decision {
    match force {
        Force::Auto => decide(file, profile, dv_strippable),
        Force::Transcode => {
            let (_, mut reasons) = evaluate(file, profile);
            reasons.insert(0, "forced transcode (manual quality)".to_owned());
            Decision {
                method: PlaybackMethod::Transcode,
                reasons,
                transcode_audio: true,
                preserve_dolby_vision: false,
                container: "mp4",
                delivered_dynamic_range: delivered_dynamic_range(
                    file,
                    PlaybackMethod::Transcode,
                    false,
                ),
            }
        }
        Force::Original => {
            // Never re-encode video: direct-play when the browser can take the
            // container + audio, else a copy-video remux. If the pick turns out
            // undecodable, the client's error path falls back to transcode.
            let (c, _) = evaluate(file, profile);
            let has_av_offset = file.audio_offset_ms != 0;
            // A DV source this client can't decode still may not be handed
            // over raw — Original means "no video re-encode", which a strip
            // remux honours (the base layer is untouched). When even that is
            // unavailable the remux is the client's error path to rescue, as
            // it always was.
            let dv = dv_handling(file, profile, dv_strippable);
            let method = if c.container_ok && c.audio_ok && !has_av_offset && dv == DvHandling::None
            {
                PlaybackMethod::DirectPlay
            } else {
                PlaybackMethod::Remux
            };
            let mut reasons = vec!["forced original quality (no video transcode)".to_owned()];
            if dv == DvHandling::Strip {
                reasons.push(
                    "Dolby Vision metadata removed for this device; compatible HDR base kept"
                        .to_owned(),
                );
            }
            let preserve_dolby_vision = is_dolby_vision(file) && profile.allows_dolby_vision(file);
            Decision {
                method,
                reasons,
                transcode_audio: !c.audio_ok,
                preserve_dolby_vision,
                container: "mp4",
                delivered_dynamic_range: delivered_dynamic_range(
                    file,
                    method,
                    preserve_dolby_vision,
                ),
            }
        }
    }
}

/// Above this, a remux wants a segmented transport even on storage nobody has
/// measured. Set to sit above 4K web-DLs (25–35 Mb/s) and below disc remuxes,
/// so the change reaches the files that are actually failing and leaves the
/// ones that work alone.
const SEGMENTED_FLOOR_BPS: f64 = 40e6;
/// ...and below this much headroom over the storage that holds it, whatever
/// the absolute bitrate. On a slow mount even a modest file needs the deeper
/// buffer, and on a fast one a big file may not.
const SEGMENTED_MIN_HEADROOM: f64 = 8.0;

/// Should a remux be delivered as HLS segments rather than progressively?
///
/// Chrome's progressive read-ahead is a hard ~2.2 seconds and no response
/// header can raise it (PERF-PLAN §4.3bis, measured). That is a fine margin
/// for ordinary web video and a fatal one for a 69 Mb/s remux, where any
/// supply gap longer than two seconds is a visible stall. The same bytes sent
/// as HLS go through MSE instead, where the buffer is the player's to set.
///
/// Video quality is identical either way — this chooses a *transport*, not a
/// ladder rung. Which is why the rule can afford to be generous: the cost of
/// a false positive is a playlist round-trip at startup, and the cost of a
/// false negative is the stall this exists to remove.
///
/// `storage_bps` is what the mount holding the file measured
/// (`storeprobe`); `None` means nobody has measured it, which falls back to
/// the absolute floor rather than assuming either way.
///
/// Returns the reason, so the player and the log can say why.
pub fn prefer_segmented(bitrate_bps: Option<i64>, storage_bps: Option<f64>) -> Option<String> {
    // An unprobed file has no bitrate, and guessing one from resolution would
    // route half a library on an inference. Leave it on the path it has.
    let bitrate = bitrate_bps.filter(|b| *b > 0)? as f64;
    if bitrate >= SEGMENTED_FLOOR_BPS {
        return Some(format!(
            "{:.0} Mb/s source — too fast for the browser's 2.2 s progressive buffer",
            bitrate / 1e6
        ));
    }
    let headroom = storage_bps.filter(|s| *s > 0.0)? / bitrate;
    (headroom < SEGMENTED_MIN_HEADROOM).then(|| {
        format!(
            "storage reads only {headroom:.1}× this file's bitrate — too little \
             margin for the browser's 2.2 s progressive buffer"
        )
    })
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
            probed: true,
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
        let d = decide(&file("mp4", "h264", "aac"), default_profile(), true);
        assert_eq!(d.method, PlaybackMethod::DirectPlay);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn audio_only_audiobook_direct_plays_on_web() {
        let mut audiobook = file("m4b", "h264", "aac");
        audiobook.video_codec = None;
        audiobook.video_profile = None;
        audiobook.width = None;
        audiobook.height = None;
        audiobook.bit_depth = None;
        audiobook.hdr = None;
        audiobook.hdr_format = None;

        let decision = decide(&audiobook, default_profile(), true);
        assert_eq!(decision.method, PlaybackMethod::DirectPlay);
        assert!(decision.reasons.is_empty());
    }

    #[test]
    fn mkv_h264_aac_remuxes_on_web() {
        // Right codecs, wrong container → remux, no audio transcode.
        let d = decide(&file("mkv", "h264", "aac"), default_profile(), true);
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(!d.transcode_audio);
    }

    #[test]
    fn mkv_h264_ac3_remuxes_with_audio_transcode() {
        let d = decide(&file("mkv", "h264", "ac3"), default_profile(), true);
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
            decide(&hevc, default_profile(), true).method,
            PlaybackMethod::Transcode
        );
        let native = profile("directplay-any").expect("profile");
        assert_eq!(
            decide(&hevc, native, true).method,
            PlaybackMethod::DirectPlay
        );
    }

    #[test]
    fn hdr_forces_transcode_on_sdr_profile() {
        let mut f = file("mp4", "h264", "aac");
        f.hdr = Some("hdr10".to_owned());
        let d = decide(&f, default_profile(), true);
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
            false,
        );
        assert_eq!(
            decide(&hevc_mp4, &caps, true).method,
            PlaybackMethod::DirectPlay
        );
        // Same codecs, MKV container → remux (copy video), not transcode.
        let hevc_mkv = file("mkv", "hevc", "aac");
        assert_eq!(decide(&hevc_mkv, &caps, true).method, PlaybackMethod::Remux);
    }

    #[test]
    fn automatic_routing_matrix_covers_every_compatibility_dimension() {
        let baseline = file("mp4", "h264", "aac");
        assert_eq!(
            decide(&baseline, default_profile(), true).method,
            PlaybackMethod::DirectPlay
        );

        let mut wrong_container = baseline.clone();
        wrong_container.container = Some("mkv".into());
        assert_eq!(
            decide(&wrong_container, default_profile(), true).method,
            PlaybackMethod::Remux
        );

        let mut wrong_audio = baseline.clone();
        wrong_audio.audio_streams[0].codec = "dts".into();
        let audio = decide(&wrong_audio, default_profile(), true);
        assert_eq!(audio.method, PlaybackMethod::Remux);
        assert!(audio.transcode_audio);

        let mut corrected_sync = baseline.clone();
        corrected_sync.audio_offset_ms = 125;
        assert_eq!(
            decide(&corrected_sync, default_profile(), true).method,
            PlaybackMethod::Remux
        );

        let mut wrong_video = baseline.clone();
        wrong_video.video_codec = Some("mpeg2video".into());
        assert_eq!(
            decide(&wrong_video, default_profile(), true).method,
            PlaybackMethod::Transcode
        );

        let mut capped = default_profile().clone();
        capped.max_height = Some(720);
        assert_eq!(
            decide(&baseline, &capped, true).method,
            PlaybackMethod::Transcode
        );

        capped.max_height = None;
        capped.max_bitrate = Some(4_000_000);
        assert_eq!(
            decide(&baseline, &capped, true).method,
            PlaybackMethod::Transcode
        );

        let mut hdr = baseline.clone();
        hdr.hdr = Some("hdr10".into());
        assert_eq!(
            decide(&hdr, default_profile(), true).method,
            PlaybackMethod::Transcode
        );

        let mut unprobed_container = baseline;
        unprobed_container.container = None;
        assert_eq!(
            decide(&unprobed_container, default_profile(), true).method,
            PlaybackMethod::Remux,
            "unknown compatibility is not permission to hand over the raw container"
        );
    }

    #[test]
    fn manual_quality_matrix_pins_auto_original_and_every_rung() {
        assert_eq!(Force::parse("auto"), Force::Auto);
        assert_eq!(Force::parse("original"), Force::Original);
        assert_eq!(Force::parse("transcode"), Force::Transcode);
        assert_eq!(Force::parse("future-value"), Force::Auto);

        let unsupported_video = file("mp4", "hevc", "aac");
        assert_eq!(
            decide_forced(&unsupported_video, default_profile(), Force::Auto, true).method,
            PlaybackMethod::Transcode
        );
        assert_eq!(
            decide_forced(
                &unsupported_video,
                default_profile(),
                Force::Original,
                true
            )
            .method,
            PlaybackMethod::DirectPlay,
            "Original keeps the source bytes when its container and audio are usable; the client owns the rescue"
        );

        let incompatible_envelope = file("mkv", "hevc", "dts");
        let original = decide_forced(
            &incompatible_envelope,
            default_profile(),
            Force::Original,
            true,
        );
        assert_eq!(original.method, PlaybackMethod::Remux);
        assert!(original.transcode_audio);

        let direct = file("mp4", "h264", "aac");
        assert_eq!(
            decide_forced(&direct, default_profile(), Force::Transcode, true).method,
            PlaybackMethod::Transcode
        );
    }

    /// The Chrome-refuses-Dolby-Vision failure, as a decision. Both films it
    /// was found on (a P7 and a P8 disc remux, both "HDR10-compatible") were
    /// remuxed to Chrome because the client claimed HDR — and Chrome refused
    /// the track outright, so the player's error path re-encoded a 4K remux
    /// down to the Auto rung. Safari played the same files untouched.
    #[test]
    fn dolby_vision_is_not_handed_to_a_browser_that_cannot_decode_it() {
        let mut dv = file("mkv", "hevc", "aac");
        dv.hdr = Some("dolby_vision".to_owned());
        dv.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".to_owned());
        let hdr_client = |dolby: bool| {
            caps_profile(
                vec!["mkv".into(), "mp4".into()],
                vec!["hevc".into(), "h264".into()],
                vec!["aac".into()],
                None,
                true,
                dolby,
            )
        };

        // Safari: decodes DV, so nothing changes — the file direct-plays.
        let safari = hdr_client(true);
        assert_eq!(
            decide(&dv, &safari, true).method,
            PlaybackMethod::DirectPlay,
            "a client that decodes Dolby Vision is handed it untouched"
        );
        assert!(decide(&dv, &safari, true).preserve_dolby_vision);

        // Chrome, on a server that can strip: a remux, not a re-encode. The
        // base layer is kept, so the viewer still gets the source's pixels —
        // but it takes ffmpeg, so the raw file may not be handed over.
        let chrome = hdr_client(false);
        let stripped = decide(&dv, &chrome, true);
        assert_eq!(stripped.method, PlaybackMethod::Remux);
        assert!(
            stripped.reasons.iter().any(|r| r.contains("Dolby Vision")),
            "and it says so: {:?}",
            stripped.reasons
        );
        assert!(!stripped.preserve_dolby_vision);

        // Chrome, on a server that cannot strip (ffmpeg < 7.1): the only
        // stream this browser will play is a re-encoded one. Deciding that up
        // front is the point — the alternative is what shipped: a remux the
        // browser refuses, then a rescue nobody asked for.
        let reencoded = decide(&dv, &chrome, false);
        assert_eq!(reencoded.method, PlaybackMethod::Transcode);
        assert!(
            reencoded.reasons.iter().any(|r| r.contains("7.1")),
            "naming the fix, not just the symptom: {:?}",
            reencoded.reasons
        );

        // An HDR10 file is untouched by any of this — the rule is about the
        // Dolby Vision configuration, not about HDR.
        let mut hdr10 = file("mp4", "hevc", "aac");
        hdr10.hdr = Some("hdr10".to_owned());
        assert_eq!(
            decide(&hdr10, &chrome, false).method,
            PlaybackMethod::DirectPlay
        );
    }

    /// Original means "no video re-encode", and a DV strip honours that — the
    /// base layer is copied. But it still cannot be direct play, because the
    /// raw file is what the browser refuses.
    #[test]
    fn forced_original_strips_dolby_vision_rather_than_handing_over_the_raw_file() {
        let mut dv = file("mp4", "hevc", "aac"); // container+audio both fine
        dv.hdr = Some("dolby_vision".to_owned());
        dv.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".to_owned());
        let chrome = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        let d = decide_forced(&dv, &chrome, Force::Original, true);
        assert_eq!(
            d.method,
            PlaybackMethod::Remux,
            "direct play would hand over the DV track the browser refuses"
        );
        assert!(
            !d.transcode_audio,
            "and the audio it can already play is copied"
        );

        let safari = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            true,
        );
        assert_eq!(
            decide_forced(&dv, &safari, Force::Original, true).method,
            PlaybackMethod::DirectPlay
        );
    }

    #[test]
    fn dolby_vision_profiles_are_negotiated_individually() {
        let mut apple = caps_profile(
            vec!["mp4".into(), "mov".into(), "m4v".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        apple.dolby_vision_profiles = vec![5, 8];
        apple.remux_dolby_vision = true;
        let mut android = caps_profile(
            vec![
                "mkv".into(),
                "mp4".into(),
                "webm".into(),
                "mov".into(),
                "ts".into(),
            ],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        android.dolby_vision_profiles = vec![5, 8];

        let mut p5 = file("mp4", "hevc", "aac");
        p5.hdr = Some("dolby_vision".to_owned());
        p5.hdr_format = Some("Dolby Vision · Profile 5".to_owned());
        let supported = decide(&p5, &apple, true);
        assert_eq!(supported.method, PlaybackMethod::Remux);
        assert!(supported.preserve_dolby_vision);
        assert_eq!(supported.delivered_dynamic_range, "dolby_vision");
        assert!(supported
            .reasons
            .iter()
            .any(|reason| reason.contains("copy-video HLS")));

        let mut p8 = file("mkv", "hevc", "aac");
        p8.hdr = Some("dolby_vision".to_owned());
        p8.hdr_format = Some("Dolby Vision · Profile 8 (HDR10-compatible)".to_owned());
        let apple_p8 = decide(&p8, &apple, true);
        assert_eq!(apple_p8.method, PlaybackMethod::Remux);
        assert!(apple_p8.preserve_dolby_vision);
        assert_eq!(apple_p8.delivered_dynamic_range, "dolby_vision");
        let android_p8 = decide(&p8, &android, true);
        assert_eq!(android_p8.method, PlaybackMethod::DirectPlay);
        assert!(android_p8.preserve_dolby_vision);
        assert_eq!(android_p8.delivered_dynamic_range, "dolby_vision");

        let mut p7 = file("mp4", "hevc", "aac");
        p7.hdr = Some("dolby_vision".to_owned());
        p7.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".to_owned());
        let fallback = decide(&p7, &apple, true);
        assert_eq!(fallback.method, PlaybackMethod::Remux);
        assert!(!fallback.preserve_dolby_vision);
        assert_eq!(fallback.delivered_dynamic_range, "hdr10");
        assert!(fallback
            .reasons
            .iter()
            .all(|reason| !reason.contains("browser")));
        let android_fallback = decide(&p7, &android, true);
        assert_eq!(android_fallback.method, PlaybackMethod::Remux);
        assert!(!android_fallback.preserve_dolby_vision);
        assert_eq!(android_fallback.delivered_dynamic_range, "hdr10");

        let sdr = caps_profile(
            vec!["mkv".into(), "mp4".into()],
            vec!["hevc".into(), "h264".into()],
            vec!["aac".into()],
            None,
            false,
            false,
        );
        let sdr_fallback = decide(&p8, &sdr, true);
        assert_eq!(sdr_fallback.method, PlaybackMethod::Transcode);
        assert!(!sdr_fallback.preserve_dolby_vision);
        assert_eq!(sdr_fallback.delivered_dynamic_range, "sdr");
    }

    /// The badge's whole reason for existing: a DV disc remux says "DV P7"
    /// while Chrome watches a tone-mapped 1080p SDR encode of it. The
    /// decision already knows which of the three deliveries happened — this
    /// pins that it now says so out loud, per delivery.
    #[test]
    fn a_dolby_vision_plan_reports_the_grade_it_actually_delivers() {
        let mut dv = file("mkv", "hevc", "aac");
        dv.hdr = Some("dolby_vision".to_owned());
        dv.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".to_owned());
        let hdr_client = |dolby: bool| {
            caps_profile(
                vec!["mkv".into(), "mp4".into()],
                vec!["hevc".into(), "h264".into()],
                vec!["aac".into()],
                None,
                true,
                dolby,
            )
        };

        // Safari decodes DV: the file goes over untouched, RPUs and all.
        let safari = decide(&dv, &hdr_client(true), true);
        assert_eq!(safari.method, PlaybackMethod::DirectPlay);
        assert_eq!(safari.delivered_dynamic_range, "dolby_vision");

        // Chrome on a server that can strip: the base layer survives, and
        // the base layer is what the compatibility marker names.
        let stripped = decide(&dv, &hdr_client(false), true);
        assert_eq!(stripped.method, PlaybackMethod::Remux);
        assert_eq!(
            stripped.delivered_dynamic_range, "hdr10",
            "an HDR10-compatible base delivers HDR10, not Dolby Vision"
        );
        let mut hlg_base = dv.clone();
        hlg_base.hdr_format = Some("Dolby Vision · Profile 8 (HLG-compatible)".to_owned());
        assert_eq!(
            decide(&hlg_base, &hdr_client(false), true).delivered_dynamic_range,
            "hlg",
            "and an HLG-compatible base delivers HLG"
        );

        // Chrome on a server that cannot strip: a re-encode, which is H.264
        // 8-bit through the tone-map graph however the source was graded.
        let reencoded = decide(&dv, &hdr_client(false), false);
        assert_eq!(reencoded.method, PlaybackMethod::Transcode);
        assert_eq!(reencoded.delivered_dynamic_range, "sdr");
    }

    /// The non-DV half of the truth table. Copied video keeps whatever the
    /// source was graded in; every transcode lands on SDR, because every
    /// encoder in the pipeline emits H.264 8-bit.
    #[test]
    fn copied_video_keeps_the_sources_grade_and_a_transcode_never_does() {
        let mut hdr10 = file("mkv", "hevc", "aac"); // MKV → container mismatch
        hdr10.hdr = Some("hdr10".to_owned());
        let hdr_client = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        let remuxed = decide(&hdr10, &hdr_client, true);
        assert_eq!(remuxed.method, PlaybackMethod::Remux);
        assert_eq!(remuxed.delivered_dynamic_range, "hdr10");

        // Same file, SDR display: the server tone-maps, and says so.
        let sdr_client = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            false,
            false,
        );
        let toned = decide(&hdr10, &sdr_client, true);
        assert_eq!(toned.method, PlaybackMethod::Transcode);
        assert_eq!(toned.delivered_dynamic_range, "sdr");

        // An SDR source is SDR wherever it goes — there is no grade to lose.
        let plain = file("mp4", "h264", "aac");
        assert_eq!(
            decide(&plain, default_profile(), true).delivered_dynamic_range,
            "sdr"
        );
        assert_eq!(
            decide(&file("mkv", "h264", "ac3"), default_profile(), true).delivered_dynamic_range,
            "sdr"
        );
    }

    /// A manual quality pick is still a delivery, so it still has to answer.
    /// Original honours the strip (base layer kept); Transcode overrides a
    /// perfectly direct-playable DV file and tone-maps it.
    #[test]
    fn a_forced_quality_reports_the_grade_that_override_delivers() {
        let mut dv = file("mp4", "hevc", "aac"); // container+audio both fine
        dv.hdr = Some("dolby_vision".to_owned());
        dv.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".to_owned());
        let chrome = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        let original = decide_forced(&dv, &chrome, Force::Original, true);
        assert_eq!(original.method, PlaybackMethod::Remux);
        assert_eq!(original.delivered_dynamic_range, "hdr10");

        let safari = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            true,
        );
        assert_eq!(
            decide_forced(&dv, &safari, Force::Original, true).delivered_dynamic_range,
            "dolby_vision",
            "Original on a client that decodes DV delivers DV"
        );
        assert_eq!(
            decide_forced(&dv, &safari, Force::Transcode, true).delivered_dynamic_range,
            "sdr",
            "and a forced rung tone-maps the same file, direct-playable or not"
        );
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
            false,
        );
        assert_eq!(decide(&f, &sdr, true).method, PlaybackMethod::Transcode); // SDR display → tone-map
        let hdr = caps_profile(
            vec!["mp4".into()],
            vec!["hevc".into()],
            vec!["aac".into()],
            None,
            true,
            false,
        );
        assert_eq!(decide(&f, &hdr, true).method, PlaybackMethod::DirectPlay); // HDR display → direct
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
            false,
        );
        assert_eq!(decide(&f, &caps, true).method, PlaybackMethod::DirectPlay);
    }

    #[test]
    fn forced_original_never_transcodes_video() {
        // HEVC the browser can't take would auto-transcode; Original forces a
        // copy-video remux instead (client rescues if it truly won't decode).
        let hevc = file("mkv", "hevc", "aac");
        let d = decide_forced(&hevc, default_profile(), Force::Original, true);
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(decide(&hevc, default_profile(), true).method == PlaybackMethod::Transcode);
    }

    #[test]
    fn forced_transcode_overrides_a_direct_playable_file() {
        let mp4 = file("mp4", "h264", "aac");
        assert_eq!(
            decide(&mp4, default_profile(), true).method,
            PlaybackMethod::DirectPlay
        );
        let d = decide_forced(&mp4, default_profile(), Force::Transcode, true);
        assert_eq!(d.method, PlaybackMethod::Transcode);
    }

    #[test]
    fn forced_auto_matches_plain_decide() {
        let mkv = file("mkv", "h264", "ac3");
        assert_eq!(
            decide_forced(&mkv, default_profile(), Force::Auto, true).method,
            decide(&mkv, default_profile(), true).method
        );
    }

    #[test]
    fn a_disc_remux_wants_segments_even_on_storage_nobody_measured() {
        let why = prefer_segmented(Some(69_000_000), None).expect("above the floor");
        assert!(why.contains("69 Mb/s"), "{why}");
    }

    #[test]
    fn an_ordinary_file_on_ample_storage_stays_progressive() {
        // 25 Mb/s off a mount reading 335 Mb/s: 13x headroom. Changing this
        // file's transport buys nothing and costs a playlist round-trip.
        assert_eq!(prefer_segmented(Some(25_000_000), Some(335e6)), None);
    }

    #[test]
    fn a_modest_file_on_slow_storage_still_wants_segments() {
        // The rule that the absolute floor alone would miss: 20 Mb/s is not a
        // big file, but off a 100 Mb/s mount it has 5x headroom, and 2.2 s of
        // buffer is thin at 5x whatever the bitrate says.
        let why = prefer_segmented(Some(20_000_000), Some(100e6)).expect("thin headroom");
        assert!(why.contains("5.0×"), "{why}");
    }

    #[test]
    fn nothing_is_claimed_about_a_file_that_was_never_probed() {
        // No bitrate means no measurement, not a small file. Guessing one from
        // resolution would reroute a library on an inference.
        assert_eq!(prefer_segmented(None, Some(100e6)), None);
        assert_eq!(prefer_segmented(Some(0), Some(100e6)), None);
        // And an unmeasured mount falls back to the floor rather than to a
        // headroom computed from a number that does not exist.
        assert_eq!(prefer_segmented(Some(20_000_000), None), None);
        assert!(prefer_segmented(Some(69_000_000), None).is_some());
    }
}
