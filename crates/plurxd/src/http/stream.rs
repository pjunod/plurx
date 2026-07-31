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
    /// `1` when this client decodes Dolby Vision (Safari does; Chrome does
    /// not, at any profile). Absent means no — an old client that never sends
    /// it is exactly one that has not been taught to ask.
    pub dv: Option<u8>,
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
                self.dv == Some(1),
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
    },
    /// Re-encode. POST `sessions_url`, omitting `height`: Auto is the
    /// server's choice because the rung depends on which encoder wins, and
    /// only the create response knows that (`TranscodeManager::auto_height`).
    Transcode { sessions_url: String },
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
    /// Skippable intro/credits regions (chapter-derived where possible).
    pub markers: Vec<Marker>,
    /// Persisted manual A/V sync correction for this file (positive = audio
    /// later). The player's sync menu edits this and restarts the stream.
    pub audio_offset_ms: i64,
    /// What the container itself declares (audio start − video start), when
    /// nonzero. Diagnostic only — declared offsets are already honored.
    pub declared_offset_ms: Option<i64>,
    /// The quality ladder for this source, top rung first (ADAPTIVE-QUALITY.md
    /// Phase 1): each rung's height, nominal wire cost, and rate-control peak,
    /// filtered to what the source can feed — so the client's quality menu and
    /// Auto controller stop hardcoding any of it.
    pub ladder: Vec<crate::transcode::Rung>,
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
/// One authority — the ffmpeg the daemon actually probed at boot — because
/// the answer decides a *verdict* now, not just a bitstream-filter argument,
/// and a second copy of it somewhere else is a second answer waiting to
/// disagree.
fn dv_strippable(state: &AppState) -> bool {
    plurx_core::transcode::ffmpeg_has_dovi_bsf(state.system.ffmpeg_version.as_deref().unwrap_or(""))
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
/// container=…&hdr=…&force=…` (runtime browser capabilities + quality override);
/// native clients still pass `?profile=`.
pub async fn decision(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<Caps>,
) -> Result<Json<DecisionResponse>, ApiError> {
    let file = load_file(&state, id).await?;
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
    let decision = q.decide(&file, dv_strippable(&state));

    let play_url = match decision.method {
        playback::PlaybackMethod::DirectPlay => format!("/api/v1/files/{id}/direct"),
        _ => format!("/api/v1/files/{id}/stream.mp4"),
    };
    let sessions_url = format!("/api/v1/files/{id}/hls/sessions");
    let delivery = match decision.method {
        playback::PlaybackMethod::DirectPlay => DeliveryPlan::Direct {
            url: play_url.clone(),
        },
        playback::PlaybackMethod::Remux => DeliveryPlan::Remux {
            url: play_url.clone(),
            sessions_url,
            aac: decision.transcode_audio,
        },
        playback::PlaybackMethod::Transcode => DeliveryPlan::Transcode { sessions_url },
    };
    // Only a remux has a transport choice to make. Direct play is already a
    // range-served file, which is the case Chrome buffers *well* — it was the
    // control in §4.3bis, at 10.8 s against the progressive path's 2.2 — and a
    // transcode is HLS already.
    let prefer_segmented = if decision.method == playback::PlaybackMethod::Remux {
        let storage = state.storage.read().await;
        let read_bps = storage.for_path(&file.path).and_then(|m| m.read_bps);
        playback::prefer_segmented(file.bitrate, read_bps)
    } else {
        None
    };
    let markers = markers_for(&state, &file).await;

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
        markers,
        audio_offset_ms: file.audio_offset_ms,
        declared_offset_ms: declared_av_offset(&state, id).await,
        ladder: crate::transcode::ladder(file.height),
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

/// Entries the subtitle cache may hold before the oldest are trimmed, and
/// how far a trim takes it. Hysteresis so the sweep runs once in a while
/// rather than on every insert at the boundary. VTTs run tens to a few
/// hundred kilobytes, so the cap is megabytes of disk, not gigabytes.
const SUBS_CACHE_MAX_ENTRIES: usize = 256;
const SUBS_CACHE_TRIM_TO: usize = 224;

/// Drop the oldest cached subtitles once the cache outgrows its cap, and any
/// abandoned temp file a crashed extraction left behind.
async fn prune_subs_cache(dir: &std::path::Path) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // A temp file older than an hour is a crashed extraction, not one in
        // flight; a live one is written and renamed within seconds.
        if name.starts_with(".tmp-") {
            if modified.elapsed().is_ok_and(|age| age.as_secs() > 3600) {
                let _ = tokio::fs::remove_file(&path).await;
            }
            continue;
        }
        entries.push((modified, path));
    }
    if entries.len() <= SUBS_CACHE_MAX_ENTRIES {
        return;
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let doomed = entries.len() - SUBS_CACHE_TRIM_TO;
    for (_, path) in entries.into_iter().take(doomed) {
        let _ = tokio::fs::remove_file(path).await;
    }
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

    let cached = state
        .subs_dir
        .join(format!("f{id}-s{index}-{}-{}.vtt", file.size, file.mtime));
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        return Ok(vtt_response(bytes));
    }

    tokio::fs::create_dir_all(&state.subs_dir)
        .await
        .map_err(|e| ApiError::Internal(format!("creating subtitle cache: {e}")))?;
    let tmp = state
        .subs_dir
        .join(format!(".tmp-{}.vtt", uuid::Uuid::new_v4()));
    let out = tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&file.path)
        .args(["-map", &format!("0:s:{index}"), "-f", "webvtt"])
        .arg(&tmp)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ApiError::Internal(format!("spawning ffmpeg: {e}")))?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let why = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            file_id = id,
            index,
            "subtitle extraction failed: {}",
            why.trim()
        );
        return Err(ApiError::Internal("subtitle extraction failed".into()));
    }
    // This response's bytes come from the temp file, so serving does not
    // depend on winning the rename.
    let bytes = tokio::fs::read(&tmp)
        .await
        .map_err(|e| ApiError::Internal(format!("reading extracted subtitles: {e}")))?;
    if tokio::fs::rename(&tmp, &cached).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    prune_subs_cache(&state.subs_dir).await;
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

/// GET /api/v1/files/:id/direct — raw file with HTTP range support.
pub async fn direct(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let served = serve_file_range(&file.path, &headers).await;
    match &served {
        // Bytes are going out: this is the moment playback is real.
        Ok(_) => crate::playstart::note_playback_started(&state, user.id, id),
        // The open failed, so whatever the availability cache believes is
        // wrong — the unmounted-share case, arriving as it actually arrives.
        Err(_) => state.availability.forget(id),
    }
    served
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
    /// `1` when this client decodes Dolby Vision (Safari does; Chrome does
    /// not, at any profile). Absent means no — an old client that never sends
    /// it is exactly one that has not been taught to ask.
    pub dv: Option<u8>,
    pub force: Option<String>,
    /// The player's own stream id, so it can ask `/stream/:id/status` how this
    /// remux is doing. Optional: an old client, curl, or an AirPlay target
    /// just gets an untracked stream.
    pub stream: Option<String>,
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
            dv: self.dv,
            force: self.force.clone(),
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
    let file = load_file(&state, id).await?;
    crate::playstart::note_playback_started(&state, user.id, id);
    let decision = q.caps().decide(&file, dv_strippable(&state));
    let audio = q.audio.unwrap_or(0).max(0);
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
    let tracked = q
        .stream
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|sid| state.streams.register(sid, user.id, id, readrate));
    remux(RemuxSpec {
        path: &file.path,
        start: q.start,
        transcode_audio: decision.transcode_audio,
        audio_index: audio,
        // Zeroed for a file with no audio track: the correction becomes an
        // `-af` when audio is transcoded, and a filter with no stream to
        // attach to is a hard ffmpeg error where the old optional map was
        // inert.
        audio_offset_ms: if file.audio_streams.is_empty() {
            0
        } else {
            file.audio_offset_ms
        },
        hevc,
        hdr: file.hdr.clone(),
        have_dovi_bsf: plurx_core::transcode::ffmpeg_has_dovi_bsf(
            state.system.ffmpeg_version.as_deref().unwrap_or(""),
        ),
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
    // Input-side seek (fast) for resume.
    if let Some(s) = start.filter(|s| *s > 0.0) {
        cmd.arg("-ss").arg(format!("{s:.3}"));
    }
    // Pace this input (see READRATE_DEFAULT). Every input gets the same
    // treatment, as with -ss: the muxer interleaves them, so an unpaced second
    // input would drag the whole pipeline back to flat-out.
    push_pacing(&mut cmd, pacing, readrate);
    cmd.arg("-i").arg(path);
    // A persisted A/V sync correction (positive = audio later). Copied audio
    // keeps the second `-itsoffset`'d input of the same file — copy moves
    // packets and filters need frames, so there is no other way in — with
    // the same input-seek so resume stays aligned (make_zero below shifts
    // all streams by one shared amount, preserving the correction). Audio
    // that is being transcoded anyway takes the correction as a filter on
    // the one input instead of a second demuxer reading the whole source
    // again (review §3.4).
    let audio_input = if audio_offset_ms != 0 && !transcode_audio {
        if let Some(s) = start.filter(|s| *s > 0.0) {
            cmd.arg("-ss").arg(format!("{s:.3}"));
        }
        push_pacing(&mut cmd, pacing, readrate);
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
    // The bitstream filter makes the stream keep hvc1's promise: no in-band
    // parameter sets (and no dead DV metadata) — same hygiene, same reasons,
    // as the segmented copy path (`hevc_copy_bsf`).
    if hevc {
        cmd.args(["-tag:v", "hvc1"]);
        cmd.args([
            "-bsf:v",
            &plurx_core::transcode::hevc_copy_bsf(hdr.as_deref(), have_dovi_bsf),
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

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "video/mp4")],
        Body::from_stream(stream),
    )
        .into_response())
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
}
