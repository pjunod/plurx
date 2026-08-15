//! Media delivery: direct-play (HTTP range serving of the raw file), remux
//! (on-the-fly fragmented MP4 via ffmpeg `-c copy`), and the execution plan
//! that sends a transcode verdict to an HLS session.

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plurx_core::domain::{ItemKind, MediaFile};
use plurx_core::playback::{self, Decision};
use plurx_core::tracks::{
    is_bitmap_subtitle, is_native_text_subtitle, is_pgs_subtitle, prefers_original_audio,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::ffmpeg::{ffmpeg_bin, ffprobe_bin, pacing_caps, PacingCaps};
use crate::state::AppState;

/// How fast a remux is allowed to run, as a multiple of real time, and how much
/// it may deliver flat-out before that limit engages.
///
/// An unpaced `-c copy` remux is a disk-to-socket pipe: it will push a two-hour
/// film into the client's buffer as fast as TCP will carry it — measured at
/// ~200x real time on a local link. The client's own buffering is the only
/// brake, so every play and every seek becomes a line-rate burst. On wired
/// gigabit that is merely rude. Over Wi-Fi it monopolises airtime for the whole
/// burst, and a client whose DHCP lease happens to need renewing during one can
/// lose the lease and then fail to get it back, because DISCOVER is broadcast at
/// the lowest basic rate and is the first thing an air-starved AP drops.
///
/// So: burst enough to fill a comfortable buffer immediately (seeks stay
/// instant), then settle to a few times real time — fast enough to absorb the
/// peaks of a variable-bitrate film and to keep building reserve, slow enough to
/// leave the link usable for everything else on it.
pub(crate) const READRATE_DEFAULT: f64 = 4.0;
/// Seconds of content delivered flat-out before the rate limit engages.
const READRATE_BURST_SECS: f64 = 30.0;
/// The source-timeline timestamp represented by local time zero in a
/// progressive remux. Kept as a response header because the body is the MP4
/// byte stream rather than a JSON session envelope.
pub(crate) const MEDIA_ORIGIN_MS_HEADER: HeaderName =
    HeaderName::from_static("x-plurx-media-origin-ms");
/// A progressive response must not wait behind the HLS copy path's full
/// five-second media-origin budget. ffmpeg is already opening the source in
/// parallel; after one second the requested seek is the safe historic answer.
const PROGRESSIVE_MEDIA_ORIGIN_PROBE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(1);

async fn bounded_progressive_media_origin<F>(
    start_seconds: f64,
    budget: std::time::Duration,
    probe: F,
) -> f64
where
    F: std::future::Future<Output = f64>,
{
    match tokio::time::timeout(budget, probe).await {
        Ok(origin) => origin,
        Err(_) => {
            tracing::warn!(
                start_seconds,
                timeout_ms = budget.as_millis() as u64,
                "progressive media-origin probe exceeded startup budget; using requested start"
            );
            start_seconds
        }
    }
}

/// The configured remux pace, in multiples of real time. `0` disables pacing.
/// Admin-settable because the right answer depends on the link: a 10GbE lab
/// wants it off, a marginal Wi-Fi bridge wants it lower.
async fn readrate_setting(state: &AppState) -> f64 {
    match state
        .store
        .get_setting(plurx_core::store::keys::STREAM_READRATE)
        .await
    {
        Ok(Some(v)) => v.trim().parse::<f64>().ok().filter(|r| *r >= 0.0),
        _ => None,
    }
    .unwrap_or(READRATE_DEFAULT)
}

/// Push `-readrate`/`-readrate_initial_burst` for one input, if this build
/// supports them and pacing is enabled. Must be called before that input's
/// `-i`, like `-ss`: these are input options.
///
/// The flags themselves come from `Pacing`, shared with the HLS builders, so
/// the two delivery paths can't drift on what pacing means. `legacy_realtime_ok`
/// is false here: a progressive remux is consumed by the browser's own
/// back-pressure, so an old ffmpeg is better off unpaced than pinned to 1x.
fn push_pacing(cmd: &mut tokio::process::Command, caps: PacingCaps, rate: f64) {
    for arg in caps.resolve(rate, READRATE_BURST_SECS, false).args() {
        cmd.arg(arg);
    }
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
        Some("m4a") | Some("m4b") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("wma") => "audio/x-ms-wma",
        Some("epub") => "application/epub+zip",
        Some("pdf") => "application/pdf",
        Some("mobi") => "application/x-mobipocket-ebook",
        Some("azw") | Some("azw3") => "application/vnd.amazon.ebook",
        Some("fb2") => "application/x-fictionbook+xml",
        Some("cbz") => "application/vnd.comicbook+zip",
        Some("cbr") => "application/vnd.comicbook-rar",
        // Home libraries serve stills through the same range-capable helper.
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        // Safari renders HEIC natively; everyone else falls back to the
        // generated JPEG thumbnail (see http/photos.rs).
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        _ => "application/octet-stream",
    }
}

/// Runtime client capabilities + a manual quality override, sent by web and
/// native players so the server only transcodes what this specific device
/// can't play.
/// All optional and back-compatible: absent caps fall back to the named
/// `profile` (default `web-h264`). CSV fields are lowercase codec/container
/// short names.
#[derive(Deserialize, Default, Clone)]
pub struct Caps {
    /// Client family and device label, used only to correlate capability and
    /// delivery diagnostics. They never influence the playback decision.
    pub client: Option<String>,
    pub device: Option<String>,
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
    /// `1` when this client decodes Dolby Vision (Safari does; Chrome does
    /// not, at any profile). Absent means no — an old client that never sends
    /// it is exactly one that has not been taught to ask.
    pub dv: Option<u8>,
    /// Comma-separated Dolby Vision profiles the client has actually probed,
    /// e.g. `5,8`. When present this is authoritative over the legacy `dv`
    /// all-or-nothing bit.
    pub dvprofile: Option<String>,
    /// `1` when supported DV profiles need a normalized copy-video HLS
    /// envelope instead of raw progressive direct play.
    pub dvhls: Option<u8>,
    /// Manual override: `auto` (default) | `original` | `transcode`.
    pub force: Option<String>,
    /// Request-local audio choice (`a:{index}`). Absent keeps the shared
    /// playback-default policy; it never writes the server setting.
    pub audio: Option<i64>,
    /// Request-local subtitle choice (`s:{index}`), or `-1` for Off. Absent
    /// keeps the shared playback-default policy.
    pub subtitle: Option<i64>,
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
            let mut profile = playback::caps_profile(
                containers,
                vcodec,
                acodec,
                self.maxheight,
                self.hdr == Some(1),
                self.dvprofile.is_none() && self.dv == Some(1),
            );
            profile.dolby_vision_profiles = csv(&self.dvprofile)
                .into_iter()
                .filter_map(|value| value.parse::<u8>().ok())
                .collect();
            profile.remux_dolby_vision = self.dvhls == Some(1);
            profile
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
    ///
    /// `dv_strippable` is the server's own capability (see
    /// [`playback::decide`]) — passed in because only the caller holds the
    /// system info that records which ffmpeg is running.
    fn decide(&self, file: &MediaFile, dv_strippable: bool) -> Decision {
        playback::decide_forced(file, &self.profile(), self.force(), dv_strippable)
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
    /// **This track has text a client can be handed.** True for every
    /// non-bitmap codec, so it answers exactly one question: can the server
    /// extract a WebVTT sidecar for it (`GET /files/{id}/subs/{index}.vtt`,
    /// a `<track>` on a direct/remux `<video>`)? Bitmap subs (PGS/VobSub) are
    /// pictures and have nothing to extract. Their non-text delivery, when
    /// available, is described independently by `overlay` below.
    ///
    /// **`text` is not permission to ask for a native HLS rendition** — see
    /// `native`. A `mov_text` or ASS/SSA track is `text: true, native: false`:
    /// the sidecar works, the rendition does not.
    pub text: bool,
    /// Can this track become a native HLS WebVTT rendition?
    ///
    /// NOT the same question as `text`, and the difference has bitten every
    /// client that assumed it was: ASS/SSA carry text, so `text` is true, but
    /// their authored positioning and typefaces do not survive WebVTT
    /// conversion — so the master never advertises them and
    /// `POST …/hls/sessions` rejects one with "the selected subtitle requires
    /// burn-in". A client that routes on `text` therefore asks for a session
    /// the server refuses, and the natural recovery from that refusal is the
    /// burn this whole arc exists to avoid.
    ///
    /// Computed by the same `is_native_text_subtitle` the HLS master and that
    /// 400 use, so there is one classifier rather than a copy of the codec
    /// list in each client. Additive: older clients ignore it.
    pub native: bool,
    /// Optional application-overlay protocol. This remains distinct from a
    /// native HLS text rendition and is omitted unless this process can serve
    /// the advertised PGS contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<&'static str>,
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

/// The server-owned execution plan for a verdict.
///
/// `method` says what was decided; this says what to *do* about it, so a
/// client executes the plan instead of re-deriving policy from the verdict —
/// which is how Android came to play transcode verdicts through a copy path
/// and Apple came to re-encode remux verdicts at a hardcoded 1080p. Every
/// URL and flag a client needs is here; the only things a client adds to a
/// session create are its own identity (`playback_id`, `request_id`) and its
/// position (`start`, `audio`).
#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DeliveryPlan {
    /// Play the original file over HTTP range.
    Direct { url: String },
    /// Copy the video untouched. `url` is the progressive fMP4 for players
    /// whose `<video>` accepts one; a player that needs HLS transport
    /// (AVPlayer) POSTs `sessions_url` with `copy: true` and this `aac`
    /// instead — same bytes, different envelope. `aac` is whether the copy
    /// session must re-encode the audio (the client said it can't take the
    /// source codec); it is the session-shaped spelling of
    /// `transcode_audio`.
    Remux {
        url: String,
        sessions_url: String,
        aac: bool,
        /// Keep Dolby Vision signaling and dynamic metadata through the copy
        /// remux. False means expose the compatible HDR base for this client.
        preserve_dolby_vision: bool,
    },
    /// Re-encode. POST `sessions_url`, omitting `height`: Auto is the
    /// server's choice because the rung depends on which encoder wins, and
    /// only the create response knows that (`TranscodeManager::auto_height`).
    Transcode { sessions_url: String },
}

/// Turn the pure playback verdict into the API execution plan every client
/// consumes. Kept pure so adding a fourth verdict cannot silently leave one
/// client inventing its own URL or session flags.
fn delivery_plan(file_id: i64, decision: &Decision) -> (String, DeliveryPlan) {
    let direct_url = format!("/api/v1/files/{file_id}/direct");
    let remux_url = format!("/api/v1/files/{file_id}/stream.mp4");
    let sessions_url = format!("/api/v1/files/{file_id}/hls/sessions");
    match decision.method {
        playback::PlaybackMethod::DirectPlay => {
            (direct_url.clone(), DeliveryPlan::Direct { url: direct_url })
        }
        playback::PlaybackMethod::Remux => (
            remux_url.clone(),
            DeliveryPlan::Remux {
                url: remux_url,
                sessions_url,
                aac: decision.transcode_audio,
                preserve_dolby_vision: decision.preserve_dolby_vision,
            },
        ),
        playback::PlaybackMethod::Transcode => (
            // Legacy clients still receive the only progressive URL in
            // `play_url`; current clients execute `delivery` and POST HLS.
            remux_url,
            DeliveryPlan::Transcode { sessions_url },
        ),
    }
}

/// Effective request-local choices returned only when the caller supplied an
/// audio or subtitle selection. Omitting both query parameters preserves the
/// existing `/decision` JSON shape for older clients.
#[derive(Serialize)]
pub struct DecisionSelection {
    pub audio_index: Option<i64>,
    pub subtitle_index: Option<i64>,
    /// True when the selected subtitle is bitmap data with no enabled
    /// application-overlay route, so showing it requires drawing into a video
    /// transcode. Text sidecars and an advertised PGS overlay remain false.
    pub subtitle_requires_burn_in: bool,
    /// The existing HDR guard refuses that burn instead of silently replacing
    /// HDR/Dolby Vision with SDR. False when no burn is needed.
    pub subtitle_burn_in_blocked_by_hdr: bool,
}

#[derive(Serialize)]
pub struct DecisionResponse {
    pub file_id: i64,
    #[serde(flatten)]
    pub decision: Decision,
    /// The URL the client should use to play, given the verdict.
    ///
    /// Legacy: predates `delivery`, and for a transcode verdict it points at
    /// the remux endpoint (the only progressive URL there is). Clients should
    /// execute `delivery`; this stays for ones that don't yet.
    pub play_url: String,
    /// What to actually do about the verdict — see [`DeliveryPlan`].
    pub delivery: DeliveryPlan,
    /// Source video/container facts for the stats overlay.
    pub source: SourceSummary,
    /// Selectable audio tracks (for the player's audio-language menu).
    pub audio: Vec<AudioTrackDto>,
    /// Selectable text subtitle tracks (served as WebVTT sidecars).
    pub subtitles: Vec<SubTrackDto>,
    /// Effective audio/subtitle choices for a selection-aware request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<DecisionSelection>,
    /// Skippable intro/credits regions (chapter-derived where possible).
    pub markers: Vec<Marker>,
    /// Initial manual A/V correction. Always zero: corrections belong to one
    /// active playback and travel on that playback's stream/session requests.
    /// Retained in the response for older client compatibility.
    pub audio_offset_ms: i64,
    /// What the container itself declares (audio start − video start), when
    /// nonzero. Diagnostic only — declared offsets are already honored.
    pub declared_offset_ms: Option<i64>,
    /// The quality ladder for this source, top rung first (ADAPTIVE-QUALITY.md
    /// Phase 1): each rung's height, nominal wire cost, and rate-control peak,
    /// filtered to what the source can feed — so the client's quality menu and
    /// Auto controller stop hardcoding any of it.
    pub ladder: Vec<crate::transcode::Rung>,
    /// Node-local sustained-throughput prior for this coarse client/network
    /// tuple. Additive and absent while the opt-in feature is disabled or the
    /// tuple has no history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_kbps: Option<u32>,
    /// Why this remux should be delivered as HLS segments rather than
    /// progressively (PERF-PLAN §4.3bis). `None` means the progressive path is
    /// fine, which is the common case.
    ///
    /// A hint and not an instruction: the server knows the bitrate and what
    /// the storage under the file reads at, but only the browser knows whether
    /// its MSE implementation will accept this codec — Chrome decodes plenty
    /// through `<video src>` that it refuses through MediaSource. So the
    /// server says "this one would be better segmented" and the client
    /// verifies before acting, falling back to the progressive path it would
    /// otherwise have used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefer_segmented: Option<String>,
}

/// Can this server remove a Dolby Vision configuration on the way out?
///
/// One authority — what the daemon *probed* of its own ffmpeg at boot, not
/// what it parsed out of a version line — because the answer decides a
/// verdict now, not just a bitstream-filter argument, and a second copy of it
/// somewhere else is a second answer waiting to disagree.
fn dv_strippable(state: &AppState) -> bool {
    state.system.dovi_rpu
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

fn sub_tracks(file: &MediaFile, overlay_enabled: bool) -> Vec<SubTrackDto> {
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
            native: is_native_text_subtitle(&s.codec),
            overlay: (overlay_enabled && is_pgs_subtitle(&s.codec))
                .then_some(crate::pgs_overlay::PROTOCOL),
        })
        .collect()
}

fn effective_audio_selection(
    file: &MediaFile,
    requested: Option<i64>,
    policy_default: Option<i64>,
) -> Result<Option<i64>, ApiError> {
    match requested {
        Some(index)
            if index >= 0 && file.audio_streams.iter().any(|track| track.index == index) =>
        {
            Ok(Some(index))
        }
        Some(_) => Err(ApiError::BadRequest("unknown audio track".into())),
        None => Ok(policy_default),
    }
}

fn effective_subtitle_selection(
    file: &MediaFile,
    requested: Option<i64>,
    policy_default: Option<i64>,
) -> Result<Option<i64>, ApiError> {
    match requested {
        Some(-1) => Ok(None),
        Some(index)
            if index >= 0
                && file
                    .subtitle_streams
                    .iter()
                    .any(|track| track.index == index) =>
        {
            Ok(Some(index))
        }
        Some(_) => Err(ApiError::BadRequest("unknown subtitle track".into())),
        None => Ok(policy_default),
    }
}

fn subtitle_requires_burn_in(
    file: &MediaFile,
    selected: Option<i64>,
    overlay_enabled: bool,
) -> bool {
    selected
        .and_then(|index| {
            file.subtitle_streams
                .iter()
                .find(|track| track.index == index)
        })
        .is_some_and(|track| {
            is_bitmap_subtitle(&track.codec) && !(overlay_enabled && is_pgs_subtitle(&track.codec))
        })
}

fn apply_selected_subtitle(
    decision: &mut Decision,
    file: &MediaFile,
    selected: Option<i64>,
    requires_burn_in: bool,
) {
    if !requires_burn_in {
        return;
    }
    let codec = selected
        .and_then(|index| {
            file.subtitle_streams
                .iter()
                .find(|track| track.index == index)
        })
        .map(|track| track.codec.as_str())
        .unwrap_or("unknown");

    // The established HDR guard refuses an SDR burn rather than silently
    // replacing HDR/Dolby Vision video. Selection-aware preflight discloses
    // that separately in `DecisionSelection`; do not add a reason for a failure
    // that did not change the returned playback method.
    if matches!(file.hdr.as_deref(), Some("dolby_vision" | "hdr10" | "hlg")) {
        return;
    }

    decision.method = playback::PlaybackMethod::Transcode;
    decision.transcode_audio = true;
    decision.preserve_dolby_vision = false;
    decision.delivered_dynamic_range = "sdr";
    decision
        .reasons
        .push(format!("selected subtitle codec {codec} requires burn-in"));
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

/// Turn an ffprobe `chapters` array into skippable intro/credits markers.
/// Pure, so the classification and the bounds checks are testable without a
/// file or a subprocess.
fn markers_from_chapters(chapters: &[serde_json::Value], duration_ms: Option<i64>) -> Vec<Marker> {
    let mut out = Vec::new();
    for ch in chapters {
        let title = ch
            .get("tags")
            .and_then(|t| t.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let Some((kind, label)) = classify_chapter(title) else {
            continue;
        };
        let at = |key: &str| -> Option<i64> {
            ch.get(key)
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0) as i64)
        };
        if let (Some(start_ms), Some(end_ms)) = (at("start_time"), at("end_time")) {
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

/// The file's skippable regions, from the chapters captured at scan time.
///
/// This used to run its own `ffprobe -show_chapters` on every single `/decision`
/// — i.e. on every press of Play, against a file that is usually on a NAS, for
/// a fact that only changes when the file does. On a cold attribute cache that
/// is seconds of dead time at the front of the click-to-first-frame path.
///
/// Chapters now come from the scan probe. A file probed before that landed has
/// no `chapters` key at all (a chapterless file has an empty array, which is a
/// different and perfectly good answer), so those get the old live probe
/// exactly once and the result is written back — the next play reads it like
/// any other. A file whose probe never succeeded has no document to graft
/// onto and simply keeps probing live; it has larger problems, and the
/// reanalyze button is the fix for them.
async fn markers_for(state: &AppState, file: &MediaFile) -> Vec<Marker> {
    if let Some(chapters) = stored_chapters(state, file.id).await {
        return markers_from_chapters(&chapters, file.duration_ms);
    }
    let probed = probe_chapters(&file.path).await;
    if let Some(chapters) = &probed {
        // Best-effort backfill: a failure here costs one more probe next time,
        // not correctness.
        if let Ok(json) = serde_json::to_string(chapters) {
            if let Err(e) = state.store.merge_file_probe_chapters(file.id, &json).await {
                tracing::warn!(file_id = file.id, error = %e, "could not cache file chapters");
            }
        }
    }
    markers_from_chapters(&probed.unwrap_or_default(), file.duration_ms)
}

/// Chapters from the stored scan probe. `None` means "this probe predates
/// chapter capture", which is distinct from `Some(vec![])` — "probed, and this
/// file genuinely has none".
async fn stored_chapters(state: &AppState, file_id: i64) -> Option<Vec<serde_json::Value>> {
    let raw = state
        .store
        .get_file_probe_json(file_id)
        .await
        .ok()
        .flatten()?;
    let probe: serde_json::Value = serde_json::from_str(&raw).ok()?;
    probe.get("chapters")?.as_array().cloned()
}

/// One live `ffprobe -show_chapters`. `None` when ffprobe failed, so the caller
/// can tell "no chapters" from "could not ask" and decline to cache the latter.
async fn probe_chapters(path: &Path) -> Option<Vec<serde_json::Value>> {
    let out = tokio::process::Command::new(ffprobe_bin())
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
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("chapters")?.as_array().cloned()
}

/// GET /api/v1/files/:id/decision — the web player sends `?vcodec=…&acodec=…&
/// container=…&hdr=…&force=…&audio=…&subtitle=…` (runtime browser
/// capabilities + request-local track/quality choices); native clients still
/// pass `?profile=`. `subtitle=-1` explicitly selects Off.
pub async fn decision(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<Caps>,
    headers: HeaderMap,
    super::network::RemoteAddress(remote): super::network::RemoteAddress,
) -> Result<Json<DecisionResponse>, ApiError> {
    let identity = super::network::identity(&headers, remote);
    let network_prior =
        super::network::stored_prior(state.store.as_ref(), user.id, identity.as_ref()).await?;
    let mut file = load_file(&state, id).await?;
    // Older builds stored this against the file. A fresh playback must never
    // inherit that historical value; its client starts at zero and carries
    // any adjustment on each stream request for this one playback only.
    file.audio_offset_ms = 0;
    // Never hand back a play URL for a file that isn't on disk — the client
    // would open a player that can never load (the unmounted-share case).
    // Cached briefly: on a cold NAS this stat is remote I/O sitting between
    // the click and the answer, for a fact that rarely changes. Presence is
    // cached, absence never is, and the open that follows stays authoritative.
    if !state.availability.is_present(id, &file.path).await {
        return Err(ApiError::Conflict(
            "this media file is missing on the server — its library path may be \
             unmounted, moved, or renamed"
                .into(),
        ));
    }

    // Select tracks before evaluating compatibility. The container's default
    // audio is only a fallback; the execution plan must describe the codec
    // selected by the same language policy exposed to the clients below.
    let prefs = state.transcode.lang_prefs().await;
    let policy_selection = plurx_core::tracks::select_tracks(
        &file.audio_streams,
        &file.subtitle_streams,
        prefers_original_audio(&file.audio_streams),
        &prefs,
    );
    let selected_audio = effective_audio_selection(&file, q.audio, policy_selection.audio_index)?;
    let selected_subtitle =
        effective_subtitle_selection(&file, q.subtitle, policy_selection.subtitle_index)?;
    let selection_requested = q.audio.is_some() || q.subtitle.is_some();
    let selected_subtitle_requires_burn =
        subtitle_requires_burn_in(&file, selected_subtitle, state.pgs_overlay_enabled);
    let subtitle_burn_in_blocked_by_hdr = selected_subtitle_requires_burn
        && matches!(file.hdr.as_deref(), Some("dolby_vision" | "hdr10" | "hlg"));
    set_selected_audio_default(&mut file.audio_streams, selected_audio);
    let mut decision = q.decide(&file, dv_strippable(&state));
    // Only an explicit subtitle choice may change the delivery verdict. An
    // audio-only request still echoes the effective policy subtitle below,
    // but delivery does not burn that track unless the caller chose it.
    if q.subtitle.is_some() {
        apply_selected_subtitle(
            &mut decision,
            &file,
            selected_subtitle,
            selected_subtitle_requires_burn,
        );
    }

    tracing::info!(
        user_id = user.id,
        username = %user.username,
        file_id = id,
        client = q.client.as_deref().unwrap_or("unknown"),
        device = q.device.as_deref().unwrap_or("unknown"),
        vcodec = q.vcodec.as_deref().unwrap_or(""),
        acodec = q.acodec.as_deref().unwrap_or(""),
        container = q.container.as_deref().unwrap_or(""),
        hdr = q.hdr.unwrap_or_default(),
        dv = q.dv.unwrap_or_default(),
        dvprofile = q.dvprofile.as_deref().unwrap_or(""),
        dvhls = q.dvhls.unwrap_or_default(),
        force = q.force.as_deref().unwrap_or("auto"),
        audio = selected_audio,
        subtitle = selected_subtitle,
        subtitle_requires_burn_in = selected_subtitle_requires_burn,
        subtitle_burn_in_blocked_by_hdr,
        source_hdr = file.hdr.as_deref().unwrap_or("sdr"),
        source_hdr_format = file.hdr_format.as_deref().unwrap_or(""),
        method = ?decision.method,
        delivered_dynamic_range = decision.delivered_dynamic_range,
        preserve_dolby_vision = decision.preserve_dolby_vision,
        reasons = ?decision.reasons,
        "playback capability decision"
    );

    let (play_url, delivery) = delivery_plan(id, &decision);
    // Only a remux has a transport choice to make. Direct play is already a
    // range-served file, which is the case Chrome buffers *well* — it was the
    // control in §4.3bis, at 10.8 s against the progressive path's 2.2 — and a
    // transcode is HLS already.
    let prefer_segmented = if decision.method == playback::PlaybackMethod::Remux {
        playback::prefer_segmented(file.bitrate)
    } else {
        None
    };
    let markers = markers_for(&state, &file).await;

    // DTO defaults and the verdict now come from the same selection above.
    let audio = audio_tracks(&file);
    let mut subtitles = sub_tracks(&file, state.pgs_overlay_enabled);
    for s in &mut subtitles {
        s.default = selected_subtitle == Some(s.index);
    }

    // No "watching now" from here. Deciding how a file would be delivered is
    // not watching it: a stream that never started had still announced itself
    // to Trakt, and a third-party call belongs nowhere near the click path.
    // The media endpoints announce it once delivery is actually happening.

    Ok(Json(DecisionResponse {
        file_id: id,
        source: source_summary(&file),
        decision,
        play_url,
        delivery,
        audio,
        subtitles,
        selection: selection_requested.then_some(DecisionSelection {
            audio_index: selected_audio,
            subtitle_index: selected_subtitle,
            subtitle_requires_burn_in: selected_subtitle_requires_burn,
            subtitle_burn_in_blocked_by_hdr,
        }),
        markers,
        audio_offset_ms: 0,
        declared_offset_ms: declared_av_offset(&state, id).await,
        ladder: crate::transcode::ladder(file.height),
        prior_kbps: network_prior.and_then(|prior| prior.sustained_kbps),
        prefer_segmented,
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

/// Deprecated compatibility endpoint. Audio sync is now session-scoped and
/// supplied on stream/session creation, so this validates and echoes an old
/// client's value without persisting it to the file or affecting future plays.
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
    Ok(Json(serde_json::json!({ "audio_offset_ms": offset })))
}

/// GET /api/v1/files/:id/subs/{index}.vtt — subtitle stream `index` as
/// WebVTT, for a native `<track>`. Auth is by `?token=` (a `<track>` element
/// can't set headers). Text subs only; a bitmap sub (PGS/VobSub) can't
/// become VTT and returns 415.
///
/// Cached (review §3.3). Extraction used to run ffmpeg over the whole source
/// — a full read of a 60 GB remux across the NAS — on *every* request, and a
/// player fetches the track on every playback of the same film. The cache
/// key is (file id, stream index, size, mtime): a replaced or re-muxed file
/// changes its fingerprint and misses, and the stale entry ages out of the
/// cap. A miss also stops buffering the whole track in memory: ffmpeg writes
/// the file itself, to a temp name renamed into place once whole — two
/// racing misses write identical bytes, and the loser's rename is a no-op
/// worth nothing to fight over.
pub async fn subtitles_vtt(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath((id, subtitle)): AxPath<(i64, String)>,
) -> Result<Response, ApiError> {
    let index = subtitle
        .strip_suffix(".vtt")
        .unwrap_or(&subtitle)
        .parse::<i64>()
        .map_err(|_| ApiError::NotFound("subtitle track"))?;
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

    let cached = crate::subtitles::ensure_vtt(&state.subs_dir, &file, index)
        .await
        .map_err(|why| {
            // Keep the endpoint's existing diagnostic while sharing the
            // extraction/cache implementation with text subtitle burns.
            tracing::warn!(file_id = id, index, "subtitle extraction failed: {why}");
            ApiError::Internal("subtitle extraction failed".into())
        })?;
    let bytes = tokio::fs::read(&cached)
        .await
        .map_err(|e| ApiError::Internal(format!("reading extracted subtitles: {e}")))?;
    Ok(vtt_response(bytes))
}

fn vtt_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// What a direct play may say about itself. Additive and optional: every
/// client that exists today sends none of it and is served exactly as before.
#[derive(Deserialize)]
pub struct DirectQuery {
    /// The player's own stream id, spelled the same as `/stream.mp4`'s so a
    /// client learns one name. It groups the *range storm* — a seeking browser
    /// makes dozens of 206 requests for one film, and without an id from the
    /// player they are grouped by file instead, which merges two simultaneous
    /// plays of one title by one person into one row. Merging is the safe
    /// error; the other direction puts phantom viewers on the activity page.
    pub stream: Option<String>,
}

/// GET /api/v1/files/:id/direct — raw file with HTTP range support.
pub async fn direct(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<DirectQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let served = serve_file_range(&file.path, &headers).await;
    match &served {
        // Bytes are going out: this is the moment playback is real. Every
        // request in the storm reports, and the registry collapses them — the
        // repetition is what keeps a live viewer listed, since a direct play
        // has no session to end and a closed tab announces nothing.
        Ok(_) => crate::playstart::note_playback_started(
            &state,
            user.id,
            &user.username,
            id,
            crate::delivery::Method::Direct,
            q.stream.as_deref(),
        ),
        // The open failed, so whatever the availability cache believes is
        // wrong — the unmounted-share case, arriving as it actually arrives.
        Err(_) => state.availability.forget(id),
    }
    served
}

/// GET /api/v1/files/:id/content — original bytes for a text book.
///
/// Separate from `direct` because opening an EPUB/PDF is not timed playback:
/// it must not announce a viewer, scrobble, or manufacture audio progress.
pub async fn book_content(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let item = state
        .store
        .get_item(file.item_id)
        .await?
        .ok_or(ApiError::NotFound("item"))?;
    if item.kind != ItemKind::Book {
        return Err(ApiError::NotFound("book content"));
    }
    serve_file_range(&file.path, &headers).await
}

// The caps fields are inlined (not `#[serde(flatten)]`ed) because axum's
// urlencoded Query decoder doesn't support flatten.
#[derive(Deserialize)]
pub struct StreamQuery {
    /// Start offset in seconds (used for resume; remux fast-seeks the input).
    pub start: Option<f64>,
    /// Which audio stream to map (`a:{audio}`). An explicit viewer choice wins;
    /// when omitted, the same language policy as `/decision` supplies it.
    pub audio: Option<i64>,
    // Same runtime-caps fields as `/decision`, so the remux copies the audio
    // when the browser can play it (vs. re-encoding to AAC needlessly).
    pub client: Option<String>,
    pub device: Option<String>,
    pub profile: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub container: Option<String>,
    pub maxheight: Option<i64>,
    pub hdr: Option<u8>,
    /// `1` when this client decodes Dolby Vision (Safari does; Chrome does
    /// not, at any profile). Absent means no — an old client that never sends
    /// it is exactly one that has not been taught to ask.
    pub dv: Option<u8>,
    pub dvprofile: Option<String>,
    pub dvhls: Option<u8>,
    pub force: Option<String>,
    /// The player's own stream id, so it can ask `/stream/:id/status` how this
    /// remux is doing. Optional: an old client, curl, or an AirPlay target
    /// just gets an untracked stream.
    pub stream: Option<String>,
    /// Manual A/V correction for this playback only (positive delays audio).
    pub audio_offset_ms: Option<i64>,
}

/// The track a progressive remux must carry. This is intentionally the same
/// rule used by `/decision` and HLS sessions: older clients are allowed to omit
/// `audio`, but that must never mean "ffmpeg's first stream" when the UI's
/// server-selected default is a different language.
fn remux_audio_index(
    audio: &[plurx_core::domain::AudioStream],
    audio_override: Option<i64>,
    prefs: &plurx_core::tracks::LangPrefs,
) -> i64 {
    if let Some(index) = audio_override.filter(|index| *index >= 0) {
        return index;
    }
    plurx_core::tracks::select_tracks(audio, &[], prefers_original_audio(audio), prefs)
        .audio_index
        .unwrap_or(0)
        .max(0)
}

/// Make the compatibility verdict inspect the track playback will actually
/// carry. The scan-time default belongs to the container, but the server's
/// language policy (or an explicit viewer choice) may select a different
/// codec. Leaving the old default in place lets a supported E-AC-3 track make
/// an unsupported preferred TrueHD track look copyable, and the failed MP4
/// remux then pushes an otherwise compatible HDR picture into the SDR rescue.
fn set_selected_audio_default(
    audio: &mut [plurx_core::domain::AudioStream],
    selected: Option<i64>,
) {
    let Some(selected) =
        selected.filter(|selected| audio.iter().any(|track| track.index == *selected))
    else {
        return;
    };
    for track in audio {
        track.default = track.index == selected;
    }
}

impl StreamQuery {
    fn caps(&self) -> Caps {
        Caps {
            client: self.client.clone(),
            device: self.device.clone(),
            profile: self.profile.clone(),
            vcodec: self.vcodec.clone(),
            acodec: self.acodec.clone(),
            container: self.container.clone(),
            maxheight: self.maxheight,
            hdr: self.hdr,
            dv: self.dv,
            dvprofile: self.dvprofile.clone(),
            dvhls: self.dvhls,
            force: self.force.clone(),
            audio: None,
            subtitle: None,
        }
    }
}

/// GET /api/v1/files/:id/stream.mp4 — fragmented-MP4 remux, optional start.
pub async fn stream_mp4(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let mut file = load_file(&state, id).await?;
    file.audio_offset_ms = if file.audio_streams.is_empty() {
        0
    } else {
        q.audio_offset_ms.unwrap_or(0).clamp(-15_000, 15_000)
    };
    crate::playstart::note_playback_started(
        &state,
        user.id,
        &user.username,
        id,
        crate::delivery::Method::Remux,
        q.stream.as_deref(),
    );
    let prefs = state.transcode.lang_prefs().await;
    let audio = remux_audio_index(&file.audio_streams, q.audio, &prefs);
    set_selected_audio_default(&mut file.audio_streams, Some(audio));
    let decision = q.caps().decide(&file, dv_strippable(&state));
    // Copy HEVC gets an `hvc1` tag so Safari's <video> accepts the fMP4 (an
    // `hev1`-tagged MKV copy otherwise plays audio-only / black in Safari).
    let hevc = matches!(file.video_codec.as_deref(), Some("hevc" | "h265"));
    let readrate = readrate_setting(&state).await;
    // A remux copies the video untouched, so the wire and the source read both
    // have to carry the file's own bitrate, sustained, for its whole length.
    // Say up front whether that is even possible on this storage. A source the
    // box cannot read fast enough reaches the viewer as a *stall*, which is
    // indistinguishable at the client from a slow encoder or a slow link — and
    // sends everyone to look at the two halves that are working.
    log_source_headroom(&state, &file, readrate).await;
    // Register before spawning so the player's first status poll — which it
    // makes as soon as the overlay opens, possibly before a frame lands — finds
    // the stream rather than a 404 it would have to distinguish from a
    // finished one.
    //
    // Registration is unconditional now, where it used to happen only for a
    // client that supplied an id. The id still decides whether *status* is
    // reachable — a server-minted one is never guessed, so nothing can ask
    // after it — but the registry is also what the activity page lists, and a
    // remux that skipped it was a viewer nobody could see. The Android client
    // sends no `stream=` on `/stream.mp4` at all.
    let sid = q
        .stream
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("srv-{}", uuid::Uuid::new_v4()));
    let tracked = Some(
        state
            .streams
            .register(&sid, user.id, &user.username, id, readrate),
    );
    remux(RemuxSpec {
        path: &file.path,
        start: q.start,
        transcode_audio: decision.transcode_audio,
        audio_index: audio,
        // Zeroed for a file with no audio track: the correction becomes an
        // `-af` when audio is transcoded, and a filter with no stream to
        // attach to is a hard ffmpeg error where the old optional map was
        // inert.
        audio_offset_ms: file.audio_offset_ms,
        hevc,
        hdr: file.hdr.clone(),
        // The probed capability, like the decision above — not the version
        // line parsed a second time somewhere else.
        have_dovi_bsf: state.system.dovi_rpu,
        preserve_dolby_vision: decision.preserve_dolby_vision,
        readrate,
        tracked,
    })
    .await
}

/// Log what this file's storage can deliver against what the file needs.
///
/// Emitted once per remux start rather than sampled during one: the numbers
/// come from the boot probe, so repeating them per chunk would add noise
/// without adding a measurement. `warn` only when the margin is genuinely
/// thin, because a line that fires on every healthy 4K start is one people
/// filter out before it ever matters.
async fn log_source_headroom(state: &AppState, file: &MediaFile, readrate: f64) {
    let (Some(bitrate), storage) = (file.bitrate, state.storage.read().await) else {
        return;
    };
    let bitrate = bitrate as f64;
    let Some(mount) = storage.for_path(std::path::Path::new(&file.path)) else {
        return;
    };
    let Some(headroom) = mount.realtime_multiple(bitrate) else {
        return;
    };
    // Under 1x the source cannot be read as fast as it must be played, and no
    // client buffer, encoder setting or pacing value can rescue that. Under
    // the configured readrate it can still play, but it can never build the
    // reserve that absorbs a hiccup — which is the shape of a stream that
    // works until the moment anything else touches the same link.
    if headroom < 1.2 {
        tracing::warn!(
            file = %file.path.display(),
            source_mbps = bitrate / 1e6,
            storage_mbps = mount.read_bps.unwrap_or_default() / 1e6,
            headroom = headroom,
            "remux source is at or beyond what its storage can read — this will stall"
        );
    } else if headroom < readrate {
        tracing::info!(
            file = %file.path.display(),
            source_mbps = bitrate / 1e6,
            storage_mbps = mount.read_bps.unwrap_or_default() / 1e6,
            headroom = headroom,
            readrate = readrate,
            "remux cannot reach the configured pace from this storage — it will run at the storage's rate and build no reserve"
        );
    }
}

/// GET /api/v1/stream/:id/status — how a progressive remux is doing.
///
/// The HLS paths answer this from the transcode session; a progressive stream
/// is not a session (see [`crate::progressive`]), so it answers from its own
/// registry. Same shape of question, same reason for asking.
pub async fn stream_status(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<crate::progressive::StreamInfo>, ApiError> {
    state
        .streams
        .status(&id, user.id)
        .map(Json)
        .ok_or(ApiError::NotFound("stream"))
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

/// One progressive remux, fully specified. A struct rather than a parameter
/// list because these are all *about the same stream* and half of them are
/// bare bools and numbers — a call site with seven positional arguments is one
/// transposition away from remuxing at the wrong pace with the wrong track.
struct RemuxSpec<'a> {
    path: &'a Path,
    start: Option<f64>,
    transcode_audio: bool,
    audio_index: i64,
    audio_offset_ms: i64,
    /// Tag the video `hvc1` so Safari accepts HEVC in MP4.
    hevc: bool,
    /// The source's HDR flavour — picks the copy bitstream filter (a Dolby
    /// Vision source also sheds its EL/RPU units; see `hevc_copy_bsf`).
    hdr: Option<String>,
    /// This ffmpeg has `dovi_rpu` (≥ 7.1), so a DV strip can also drop the
    /// DOVI side data and with it the `dvcC` box VideoToolbox chokes on.
    have_dovi_bsf: bool,
    /// Preserve a client-supported Dolby Vision profile instead of stripping
    /// its configuration and RPU metadata to the compatible HDR base.
    preserve_dolby_vision: bool,
    readrate: f64,
    /// Telemetry handle and its registration, when the client asked to be able
    /// to watch this stream's health.
    tracked: Option<(
        std::sync::Arc<crate::progressive::Stream>,
        crate::progressive::StreamGuard,
    )>,
}

async fn remux(spec: RemuxSpec<'_>) -> Result<Response, ApiError> {
    let RemuxSpec {
        path,
        start,
        transcode_audio,
        audio_index,
        audio_offset_ms,
        hevc,
        hdr,
        have_dovi_bsf,
        preserve_dolby_vision,
        readrate,
        tracked,
    } = spec;
    let pacing = pacing_caps().await;
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    // Telemetry goes to stderr, not stdout: stdout is the MP4. The stderr
    // reader below already exists to surface remux failures, and progress
    // lines are `key=value` — trivially separable from ffmpeg's prose.
    if tracked.is_some() {
        cmd.arg("-progress").arg("pipe:2");
    }
    // Input-side seek (fast) for resume. Copied video starts at the preceding
    // keyframe, so retain the matching audio preroll as well; accurate seek
    // would discard it when audio is being encoded and desynchronise the two.
    if let Some(s) = start.filter(|s| *s > 0.0) {
        cmd.args(plurx_core::transcode::copy_input_seek_args(s));
    }
    // Pace this input (see READRATE_DEFAULT). Every input gets the same
    // treatment, as with -ss: the muxer interleaves them, so an unpaced second
    // input would drag the whole pipeline back to flat-out.
    push_pacing(&mut cmd, pacing, readrate);
    cmd.arg("-i").arg(path);
    // This playback's A/V sync correction (positive = audio later). Copied audio
    // keeps the second `-itsoffset`'d input of the same file — copy moves
    // packets and filters need frames, so there is no other way in — with
    // the same input-seek so resume stays aligned (make_zero below shifts
    // all streams by one shared amount, preserving the correction). Audio
    // that is being transcoded anyway takes the correction as a filter on
    // the one input instead of a second demuxer reading the whole source
    // again (review §3.4).
    let audio_input = if audio_offset_ms != 0 && !transcode_audio {
        if let Some(s) = start.filter(|s| *s > 0.0) {
            cmd.args(plurx_core::transcode::copy_input_seek_args(s));
        }
        push_pacing(&mut cmd, pacing, readrate);
        cmd.arg("-itsoffset")
            .arg(format!("{:.3}", audio_offset_ms as f64 / 1000.0));
        cmd.arg("-i").arg(path);
        1
    } else {
        0
    };
    // Optional video + the chosen audio track, no subtitles into the MP4.
    // Audio-only books share this remux path when their source codec needs AAC.
    cmd.args([
        "-map",
        "0:v:0?",
        "-map",
        &format!("{audio_input}:a:{audio_index}?"),
        "-sn",
    ]);
    cmd.args(["-c:v", "copy"]);
    // Safari only decodes HEVC in MP4 when the sample entry is tagged `hvc1`;
    // MKV HEVC is commonly `hev1`, which Safari renders black. Harmless for a
    // stream that's already hvc1. Video-stream-scoped so H.264 is untouched.
    // The bitstream filter makes the stream keep hvc1's promise: no in-band
    // parameter sets (and no dead DV metadata) — same hygiene, same reasons,
    // as the segmented copy path (`hevc_copy_bsf`).
    if hevc {
        cmd.args([
            "-tag:v",
            plurx_core::transcode::hevc_copy_tag(hdr.as_deref(), preserve_dolby_vision),
        ]);
        cmd.args([
            "-bsf:v",
            &plurx_core::transcode::hevc_copy_bsf_for_client(
                hdr.as_deref(),
                have_dovi_bsf,
                preserve_dolby_vision,
            ),
        ]);
    }
    if transcode_audio {
        if let Some(af) = plurx_core::transcode::audio_offset_filter(audio_offset_ms) {
            cmd.arg("-af").arg(af);
        }
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

    // Probe after the remux starts opening the source, matching the HLS copy
    // path: the work overlaps instead of adding its full latency to startup.
    // `make_zero` makes the preceding keyframe local time zero, so the
    // requested seek is not an accurate source-time origin for copied video.
    let start_seconds = start.unwrap_or(0.0).max(0.0);
    let media_origin_seconds = bounded_progressive_media_origin(
        start_seconds,
        PROGRESSIVE_MEDIA_ORIGIN_PROBE_TIMEOUT,
        crate::transcode::probe_media_origin(path, start_seconds),
    )
    .await;

    let (tracked_stream, guard) = match tracked {
        Some((s, g)) => (Some(s), Some(g)),
        None => (None, None),
    };

    // Surface remux failures: ffmpeg runs at -loglevel error, so a codec/copy
    // problem (e.g. jellyfin-ffmpeg refusing a stream the old build accepted)
    // otherwise yields an empty pipe and a blank player with nothing logged.
    // When tracked, the same pipe carries `-progress` telemetry; progress lines
    // are keyed `key=value` and everything else is still an error worth logging.
    if let Some(stderr) = child.stderr.take() {
        let telemetry = tracked_stream.as_ref().map(|s| {
            let p = std::sync::Arc::clone(&s.progress);
            // One attempt per stream — a progressive remux never respawns —
            // so a single generation is taken here and quoted for its life.
            let generation = p.begin_attempt();
            (p, generation)
        });
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some((progress, generation)) = &telemetry {
                    if is_progress_line(&line) {
                        crate::transcode::apply_progress_line(progress, *generation, &line);
                        continue;
                    }
                }
                tracing::warn!("remux ffmpeg: {line}");
            }
        });
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("ffmpeg stdout unavailable".into()))?;

    // Stream ffmpeg stdout; the Child rides along in the stream state and is
    // killed (kill_on_drop) if the client disconnects mid-stream. The
    // registration guard rides along too, so the stream deregisters at exactly
    // the moment its ffmpeg dies rather than on a timer that could outlive it.
    let reader = tokio::io::BufReader::new(stdout);
    let state = (child, reader, tracked_stream, guard);
    let stream =
        futures_util::stream::unfold(state, |(child, mut reader, tracked, guard)| async move {
            let mut buf = vec![0u8; 64 * 1024];
            match reader.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    // Bytes are counted here — where they actually leave — rather
                    // than at the top of the response. On a paced remux this is the
                    // delivery rate a viewer is really getting, gaps included.
                    if let Some(s) = &tracked {
                        s.delivery.note(n as u64);
                    }
                    Some((
                        Ok::<_, std::io::Error>(bytes::Bytes::from(buf)),
                        (child, reader, tracked, guard),
                    ))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "remux stream read error");
                    None
                }
            }
        });

    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "video/mp4")],
        Body::from_stream(stream),
    )
        .into_response();
    let media_origin_ms = (media_origin_seconds * 1000.0).round() as i64;
    response.headers_mut().insert(
        MEDIA_ORIGIN_MS_HEADER,
        HeaderValue::from_str(&media_origin_ms.to_string())
            .expect("a signed integer is a valid HTTP header value"),
    );
    Ok(response)
}

/// Is this stderr line one of ffmpeg's `-progress` blocks rather than a
/// diagnostic? Progress is strictly `lower_snake_key=value`; ffmpeg's own
/// messages are prose and normally carry a `[component @ 0x…]` prefix, so the
/// two never collide — and a misfiled line costs a log entry, not correctness.
fn is_progress_line(line: &str) -> bool {
    match line.split_once('=') {
        Some((key, _)) => {
            let key = key.trim();
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(method: playback::PlaybackMethod) -> Decision {
        Decision {
            method,
            reasons: Vec::new(),
            transcode_audio: true,
            preserve_dolby_vision: true,
            container: "mp4",
            delivered_dynamic_range: "dolby_vision",
        }
    }

    #[test]
    fn every_verdict_has_one_server_owned_execution_plan() {
        let (legacy, direct) = delivery_plan(42, &planned(playback::PlaybackMethod::DirectPlay));
        assert_eq!(legacy, "/api/v1/files/42/direct");
        assert!(matches!(
            direct,
            DeliveryPlan::Direct { url } if url == "/api/v1/files/42/direct"
        ));

        let (legacy, remux) = delivery_plan(42, &planned(playback::PlaybackMethod::Remux));
        assert_eq!(legacy, "/api/v1/files/42/stream.mp4");
        assert!(matches!(
            remux,
            DeliveryPlan::Remux {
                url,
                sessions_url,
                aac: true,
                preserve_dolby_vision: true,
            } if url == "/api/v1/files/42/stream.mp4"
                && sessions_url == "/api/v1/files/42/hls/sessions"
        ));

        let (legacy, transcode) = delivery_plan(42, &planned(playback::PlaybackMethod::Transcode));
        assert_eq!(legacy, "/api/v1/files/42/stream.mp4");
        assert!(matches!(
            transcode,
            DeliveryPlan::Transcode { sessions_url }
                if sessions_url == "/api/v1/files/42/hls/sessions"
        ));
    }

    #[test]
    fn remux_omission_uses_the_preferred_audio_instead_of_stream_zero() {
        use plurx_core::domain::AudioStream;

        let track = |index: i64, language: &str, default: bool| AudioStream {
            index,
            codec: "eac3".into(),
            channels: Some(6),
            language: Some(language.into()),
            title: None,
            default,
        };
        // Scary Movie's shape: the mux calls Italian default, while the server
        // preference and `/decision` select English stream 2.
        let audio = vec![
            track(0, "ita", true),
            track(1, "ita", true),
            track(2, "eng", false),
            track(3, "eng", false),
        ];
        let prefs = plurx_core::tracks::LangPrefs::default();

        assert_eq!(remux_audio_index(&audio, None, &prefs), 2);
        assert_eq!(remux_audio_index(&audio, Some(3), &prefs), 3);
    }

    #[test]
    fn preferred_truehd_audio_is_transcoded_for_android_hdr_remux() {
        use plurx_core::domain::AudioStream;

        let mut file = MediaFile {
            id: 5698,
            item_id: 1,
            path: "/movies/Michael (2026).mkv".into(),
            size: 1,
            mtime: 1,
            duration_ms: Some(120_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main 10".into()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            hdr: Some("dolby_vision".into()),
            hdr_format: Some("Dolby Vision · Profile 7 (HDR10-compatible)".into()),
            bitrate: Some(90_892_368),
            audio_streams: vec![
                AudioStream {
                    index: 0,
                    codec: "eac3".into(),
                    channels: Some(8),
                    language: Some("fra".into()),
                    title: Some("French E-AC-3".into()),
                    default: true,
                },
                AudioStream {
                    index: 3,
                    codec: "truehd".into(),
                    channels: Some(8),
                    language: Some("eng".into()),
                    title: Some("English TrueHD Atmos".into()),
                    default: false,
                },
            ],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
            probed: true,
        };
        let caps = Caps {
            vcodec: Some("h264,hevc,av1,vp9".into()),
            acodec: Some("aac,mp3,opus,flac,ac3,eac3".into()),
            container: Some("mp4,webm".into()),
            hdr: Some(1),
            dvprofile: Some("5,8".into()),
            ..Default::default()
        };
        let prefs = plurx_core::tracks::LangPrefs::default();

        let selected = remux_audio_index(&file.audio_streams, None, &prefs);
        assert_eq!(selected, 3, "English preference selects the TrueHD track");
        set_selected_audio_default(&mut file.audio_streams, Some(selected));
        let english = caps.decide(&file, true);
        assert_eq!(english.method, playback::PlaybackMethod::Remux);
        assert!(english.transcode_audio, "TrueHD must become AAC in MP4");
        assert_eq!(english.delivered_dynamic_range, "hdr10");
        assert!(english
            .reasons
            .iter()
            .any(|reason| reason.contains("audio codec truehd unsupported")));

        set_selected_audio_default(&mut file.audio_streams, Some(0));
        let french = caps.decide(&file, true);
        assert!(!french.transcode_audio, "the E-AC-3 alternative can copy");
    }

    #[test]
    fn explicit_dolby_vision_profiles_override_the_legacy_all_profiles_bit() {
        let caps = Caps {
            vcodec: Some("hevc".into()),
            acodec: Some("aac".into()),
            container: Some("mp4".into()),
            hdr: Some(1),
            // Kept for compatibility with an older server, which ignores the
            // new profile list. A new server must prefer the specific list.
            dv: Some(1),
            dvprofile: Some("5,8".into()),
            dvhls: Some(1),
            ..Default::default()
        };

        let profile = caps.profile();
        assert!(!profile.supports_dolby_vision);
        assert_eq!(profile.dolby_vision_profiles, vec![5, 8]);
        assert!(profile.remux_dolby_vision);
    }

    /// ffmpeg's `-progress` shares stderr with its diagnostics here, because
    /// stdout is the MP4. Telling them apart is the whole trick, and getting it
    /// wrong in either direction is silent: a misread error line vanishes from
    /// the log, a misread progress line does nothing at all.
    #[test]
    fn progress_blocks_are_distinguishable_from_ffmpegs_prose() {
        for line in [
            "out_time_us=5960000",
            "speed=4.02x",
            "frame=142",
            "progress=continue",
            "bitrate=N/A",
        ] {
            assert!(is_progress_line(line), "progress: {line}");
        }
        for line in [
            "[matroska @ 0x55f4] Could not find codec parameters",
            "Error opening input file /media/x.mkv.",
            "Conversion failed!",
            "",
            // An ffmpeg message that happens to contain '=' is still prose:
            // the key side has spaces and capitals, which progress keys never do.
            "[out#0/mp4 @ 0x1] Output file is empty, nothing was encoded",
        ] {
            assert!(!is_progress_line(line), "prose: {line}");
        }
    }

    /// This is the response-start budget in isolation: an artificially cold
    /// origin probe cannot add its full delay before the progressive body is
    /// returned. The production call uses the pinned one-second budget; the
    /// short test budget keeps the suite fast while exercising the same race.
    #[tokio::test]
    async fn delayed_origin_probe_cannot_hold_progressive_first_byte_past_budget() {
        assert_eq!(
            PROGRESSIVE_MEDIA_ORIGIN_PROBE_TIMEOUT,
            std::time::Duration::from_secs(1)
        );
        let budget = std::time::Duration::from_millis(20);
        let began = std::time::Instant::now();
        let origin = bounded_progressive_media_origin(42.0, budget, async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            40.0
        })
        .await;
        assert_eq!(origin, 42.0, "timeout falls back to the requested seek");
        assert!(
            began.elapsed() < std::time::Duration::from_millis(100),
            "the delayed probe held response creation for {:?}",
            began.elapsed()
        );

        assert_eq!(
            bounded_progressive_media_origin(42.0, budget, async { 40.0 }).await,
            40.0,
            "a prompt keyframe answer still reaches the response header"
        );
    }

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
        assert_eq!(content_type(Path::new("a.m4b")), "audio/mp4");
        assert_eq!(content_type(Path::new("a.aac")), "audio/aac");
        assert_eq!(content_type(Path::new("a.mp3")), "audio/mpeg");
        assert_eq!(content_type(Path::new("a.flac")), "audio/flac");
        assert_eq!(content_type(Path::new("a.epub")), "application/epub+zip");
        assert_eq!(content_type(Path::new("a.pdf")), "application/pdf");
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

    fn chapter(title: &str, start: &str, end: &str) -> serde_json::Value {
        serde_json::json!({ "start_time": start, "end_time": end, "tags": { "title": title } })
    }

    /// Markers are built from a chapters array, whatever produced it — which is
    /// what lets the play path read them out of the scan probe instead of
    /// running its own ffprobe against the NAS on every click.
    #[test]
    fn markers_come_from_a_chapters_array() {
        let chapters = vec![
            chapter("Opening", "0.000", "85.000"),
            chapter("The Heist", "85.000", "3000.000"),
            chapter("End Credits", "3000.000", "3180.000"),
        ];
        let m = markers_from_chapters(&chapters, Some(3_180_000));
        assert_eq!(m.len(), 2, "only intro and credits are markers");
        assert_eq!(m[0].kind, "intro");
        assert_eq!((m[0].start_ms, m[0].end_ms), (0, 85_000));
        assert!(m[0].chapter, "came from a real chapter");
        assert_eq!(m[1].kind, "credits");
        assert_eq!(m[1].start_ms, 3_000_000);

        // A chapterless file still offers Skip Credits, flagged as a guess.
        let m = markers_from_chapters(&[], Some(45 * 60_000));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, "credits");
        assert!(!m[0].chapter, "the duration heuristic is not a chapter");

        // Nothing to guess from, and nothing invented.
        assert!(markers_from_chapters(&[], None).is_empty());
        assert!(markers_from_chapters(&[], Some(60_000)).is_empty());

        // Malformed entries are skipped rather than trusted: a zero-length or
        // backwards chapter would render as an un-dismissable skip button.
        let junk = vec![
            chapter("Intro", "10.0", "10.0"),
            chapter("Intro", "90.0", "10.0"),
            serde_json::json!({ "tags": { "title": "Intro" } }),
        ];
        assert!(markers_from_chapters(&junk, None).is_empty());
    }

    /// The backfill writes chapters into the stored probe without disturbing
    /// what else is in it — and refuses to invent a document for a file whose
    /// probe never succeeded, because `probe_json IS NULL` is the fingerprint
    /// the repair job keys on.
    #[tokio::test]
    async fn chapters_backfill_into_the_stored_probe() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};
        use plurx_core::store::{SqliteStore, Store};
        let store: std::sync::Arc<dyn Store> =
            std::sync::Arc::new(SqliteStore::open_in_memory().expect("store"));
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let item = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        // A pre-chapters probe: real JSON, no `chapters` key.
        let legacy = store
            .upsert_file(
                item,
                "/media/legacy.mkv",
                1,
                1,
                &ProbeResult {
                    duration_ms: Some(600_000),
                    raw_json: Some(r#"{"format":{"duration":"600.0"},"streams":[]}"#.into()),
                    ..Default::default()
                },
            )
            .await
            .expect("legacy file");
        // A file whose probe failed outright: probe_json IS NULL.
        let unprobed = store
            .upsert_file(item, "/media/broken.mkv", 1, 1, &ProbeResult::default())
            .await
            .expect("unprobed file");

        let chapters = serde_json::to_string(&vec![chapter("Intro", "0.0", "60.0")]).expect("json");
        store
            .merge_file_probe_chapters(legacy, &chapters)
            .await
            .expect("merge");
        store
            .merge_file_probe_chapters(unprobed, &chapters)
            .await
            .expect("merge on a null probe is a no-op, not an error");

        let raw = store
            .get_file_probe_json(legacy)
            .await
            .expect("read")
            .expect("has probe json");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let stored = v
            .get("chapters")
            .and_then(|c| c.as_array())
            .expect("grafted");
        assert_eq!(stored.len(), 1);
        assert_eq!(markers_from_chapters(stored, None)[0].kind, "intro");
        // The rest of the document survived the graft.
        assert!(v.get("format").is_some(), "format survived");
        assert!(v.get("streams").is_some(), "streams survived");

        // The failed probe is still recognisably a failed probe.
        assert!(
            store
                .get_file_probe_json(unprobed)
                .await
                .expect("read")
                .is_none(),
            "a null probe must stay null — the repair job keys on it"
        );
    }

    #[test]
    fn native_is_not_the_same_question_as_text() {
        fn sub(index: i64, codec: &str) -> plurx_core::domain::SubtitleStream {
            plurx_core::domain::SubtitleStream {
                index,
                codec: codec.into(),
                ..Default::default()
            }
        }
        let file = MediaFile {
            id: 1,
            item_id: 1,
            path: "/media/anime.mkv".into(),
            size: 1,
            mtime: 1,
            duration_ms: Some(1_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(1_000),
            audio_streams: vec![],
            subtitle_streams: vec![
                sub(0, "subrip"),
                sub(1, "ass"),
                sub(2, "ssa"),
                sub(3, "hdmv_pgs_subtitle"),
                sub(4, "webvtt"),
            ],
            scanned_at: 0,
            audio_offset_ms: 0,
            probed: true,
        };
        let tracks = sub_tracks(&file, false);

        // SRT and WebVTT: text, and servable as a rendition.
        assert!(tracks[0].text && tracks[0].native);
        assert!(tracks[4].text && tracks[4].native);

        // The trap. ASS/SSA carry text, so `text` is true — but their authored
        // styling does not survive WebVTT, the master never advertises them,
        // and `POST …/hls/sessions` rejects one. A client routing on `text`
        // asks for a session the server refuses, and the natural recovery from
        // that refusal is a burn.
        assert!(tracks[1].text, "ASS carries text");
        assert!(!tracks[1].native, "ASS cannot be a native rendition");
        assert!(tracks[2].text && !tracks[2].native, "SSA likewise");

        // Bitmap is neither, and always was.
        assert!(!tracks[3].text && !tracks[3].native);
        assert!(subtitle_requires_burn_in(&file, Some(3), false));
        assert!(!subtitle_requires_burn_in(&file, Some(0), false));

        // Default-off and old servers remain wire-compatible: the additive
        // field is absent rather than null.
        let disabled = serde_json::to_value(&tracks).expect("subtitle DTOs");
        assert!(disabled
            .as_array()
            .expect("array")
            .iter()
            .all(|track| track.get("overlay").is_none()));

        let enabled = sub_tracks(&file, true);
        assert_eq!(enabled[3].overlay, Some("pgs-v1"));
        assert!(
            !subtitle_requires_burn_in(&file, Some(3), true),
            "an enabled PGS overlay keeps the video bytes untouched"
        );
        assert!(enabled
            .iter()
            .enumerate()
            .all(|(index, track)| index == 3 || track.overlay.is_none()));

        // And the field agrees with the classifier the master and the 400 use,
        // rather than being a second opinion about the same codecs.
        for (track, source) in tracks.iter().zip(&file.subtitle_streams) {
            assert_eq!(
                track.native,
                plurx_core::tracks::is_native_text_subtitle(&source.codec)
            );
        }
    }
}
