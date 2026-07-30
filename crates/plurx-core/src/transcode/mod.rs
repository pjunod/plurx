//! Transcode pipeline: pick an encoder, build the ffmpeg argument graph.
//!
//! The verdict of whether to transcode comes from [`crate::playback`]; this
//! module turns "transcode this file for that profile" into a concrete ffmpeg
//! invocation that produces HLS. Hardware *encode* is the big CPU win and is
//! selected per detected capability (NVENC/QSV/VAAPI/VideoToolbox), with a
//! software x264 fallback; HDR→SDR tone-mapping and subtitle burn-in run as
//! filters (ARCHITECTURE §3).
//!
//! The video path between decode and encode is a [`Pipeline`] — either the CPU
//! chain that downloads every frame to tone-map it in float, or one of the
//! graphs that keeps frames on the GPU. Which a node uses is decided by probe,
//! not by version (PERF-PLAN §5).

mod encoder;
mod pipeline;
mod recipe;

pub use encoder::{detect_encoders, Encoder, EncoderCaps};
pub use pipeline::{Pipeline, CANDIDATES as PIPELINE_CANDIDATES};
pub use recipe::{PipelineDigest, Recipe};

use crate::domain::MediaFile;

/// Segment length for on-the-fly HLS, in seconds. Keyframes are forced to
/// align to this so segments are independently decodable.
///
/// This is the floor under time-to-first-frame, and it is a hard one: a player
/// cannot start on half a segment, so nothing plays until one whole segment
/// exists. At 4 s a session that encodes at 1× spent four seconds producing
/// the thing the viewer is waiting for, before any of the rest of the start
/// path even begins. Two halves that (PERF-PLAN §4.4).
///
/// The costs are real and small: twice the playlist churn and segment
/// requests (nothing at these sizes), a denser forced-keyframe grid worth a
/// few percent of compression at a given bitrate, and double the per-segment
/// mux overhead. The copy path cannot force keyframes at all — there
/// `hls_time` is only a floor and real segmentation follows the source's GOP —
/// so 2 s helps copy when the source GOP is short and never hurts it.
///
/// It is also the unit of every boundary the cluster failover contract talks
/// about (`docs/PHASE3-SPIKE.md`): a session restarted on another node resumes
/// at `N * SEGMENT_SECONDS`. Nothing may hardcode the number — the spike
/// measured 4, this is now 2, and the property holds for any fixed length. A
/// second copy of the value is a failover bug waiting for the day it changes
/// again.
pub const SEGMENT_SECONDS: u32 = 2;

/// Segment floor for the COPY path, and deliberately not [`SEGMENT_SECONDS`].
///
/// On a copy, `hls_time` is a floor under cuts that can only happen at source
/// keyframes — and every cut costs a frame. The one-stream experiment
/// (docs/STUTTER-4K.md §5.3ter) proved it: the same open-GOP bitstream plays
/// 1781 frames with ZERO drops as one continuous stream, and drops exactly
/// one leading picture per segment start when fragmented, in Chrome's MSE
/// and Safari's native player alike. Only the segment-START keyframe gets
/// the random-access treatment; keyframes inside a segment decode
/// continuously and keep their leading pictures. So boundary count is the
/// drop count, and a 6 s floor cuts it ~3.4× against the ~1.75 s GOP grid.
///
/// Why 6 and not more: hls.js fetches whole segments into MediaSource, and
/// at the ~100 Mb/s this path carries a segment must stay comfortably inside
/// the byte-budgeted forward buffer (`bufferTargets`) — 6 s at 69 Mb/s is
/// ~52 MB against a 96 MB forward budget; 10 s would make one segment the
/// entire buffer. Why the transcode path stays at 2: its encoder produces at
/// ~1×, so a viewer waits for the whole first segment (TTFF), and its closed
/// GOP has no leading pictures to lose. The copy path bursts the first 90 s
/// at disk speed, so a 6 s first segment still exists in well under a
/// second.
///
/// Safe to diverge from [`SEGMENT_SECONDS`]: copy sessions are live-only —
/// never cached (no recipe hash to collide with) and outside the Phase 3
/// failover contract, which is about transcode sessions.
pub const COPY_SEGMENT_SECONDS: u32 = 6;

/// Hard byte ceiling on one copy-path segment.
///
/// The segmenter (`crate::fmp4`) holds fragments back until it finds a
/// keyframe a player can start on without discarding a leading picture. On a
/// stretch of film that offers no such point, "hold back" has to stop
/// somewhere, and bytes are the binding constraint here long before seconds
/// are: at the ~60 Mb/s a 4K remux carries, a segment is 7.5 MB a second.
///
/// **This number is the search window, and it was measured twice.** It first
/// shipped at 48 MB, chosen as half of `bufferTargets`' 96 MB forward budget
/// so MSE could hold two whole segments. That reasoning was arithmetic, and
/// the arithmetic missed something: at 58 Mb/s a 48 MB ceiling arrives 6.6
/// seconds in, so past the six-second floor the segmenter had **0.6 seconds**
/// — less than one GOP — to find a clean keyframe in. On *Wicked* (2024),
/// whose clean points come every 2.3 s, that produced 5.9 dropped frames a
/// minute against ffmpeg's own muxer at 8.0: a segmenter barely beating the
/// thing it replaced, on a source full of the cut points it was looking for.
/// `scripts/gop-census --sweep` is the instrument that showed it.
///
/// 64 MB opens the window to 2.8 seconds — about three and a half GOPs — and
/// takes the same file to 1.9 drops a minute, 4.2x better than the muxer. The
/// ceiling on going further is the SourceBuffer quota, and it is a
/// measurement rather than a budget: a real hls.js in headless Chromium, fed
/// a 59.6 Mb/s stream cut at each size with the shipped `bufferTargets()`
/// numbers (13 s forward / 4 s back), reports zero quota events at 48 and 64
/// MB, and `bufferFullError` at 80 MB and above — repeatably, in every run.
/// Peak buffered bytes were identical (155 MB) in all four arms, so what
/// fails at 80 MB is not the total: it is the size of a single append, which
/// is a limit no byte budget on the client side can be traded against.
///
/// So 64 MB is the last rung that measured clean, and the "two whole
/// segments" framing it replaces was never the thing that mattered.
pub const COPY_SEGMENT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Duration floor for the FIRST copy segment, and only the first.
///
/// A player starting a live EVENT playlist has exactly one segment of runway,
/// because that is all the playlist holds when it loads it; its next chance to
/// learn any other segment exists is one `EXT-X-TARGETDURATION` away. So the
/// first segment being long is not a small cost — it is the whole startup
/// buffer. Two seconds means the first reload lands with time to spare, and
/// by then the session's burst has filled the playlist and the question never
/// arises again.
///
/// It also puts the first frame on screen sooner: nothing plays until one
/// whole segment exists.
///
/// Costs at most one extra boundary per session, and usually not even that —
/// the cut still has to land on a keyframe with nothing to discard, which on a
/// source with clean points every couple of seconds it will.
pub const COPY_FIRST_SEGMENT_SECONDS: u32 = 2;

/// Secondary ceiling, in seconds, for sources whose bytes never pile up.
///
/// A 3 Mb/s home video would reach [`COPY_SEGMENT_MAX_BYTES`] after two
/// minutes, and `EXT-X-TARGETDURATION` has to be declared up front and may
/// never decrease — so without a duration ceiling the playlist would have to
/// promise something absurd on session one to stay honest on session two.
/// Fifteen seconds keeps the tag believable and still leaves the floor plenty
/// of room to find a clean point on any normal GOP grid.
///
/// The two ceilings meet at about 34 Mb/s (64 MB ÷ 15 s), and they meet
/// rather than cross: below that the duration ceiling binds and a segment is
/// under 64 MB, above it the byte ceiling binds and a segment is under 15 s.
/// So one published segment is at most [`COPY_SEGMENT_MAX_BYTES`] plus the
/// fragment that crossed it, at any bitrate — which is the property the
/// SourceBuffer measurement was taken against.
///
/// It spent a day at 6 s, chasing a once-per-film freeze, and came back. The
/// freeze was the client starting at the live edge and catching the producer
/// — shrinking segments only scaled it down (8.8 s frozen at 9.2 s in became
/// 5.5 s at 6.0 s), because the freeze length *is* one segment's production
/// time. The publish gate ([`COPY_PUBLISH_GATE_SEGMENTS`]) removes it
/// outright, at which point a 6 s ceiling was pure cost: boundaries on every
/// low-bitrate source, each one a dropped frame on an open-GOP disc, buying
/// nothing.
pub const COPY_SEGMENT_MAX_SECS: u32 = 15;

/// Seconds of media that must exist before the first `index.m3u8` is
/// published.
///
/// **A playlist that starts at the live edge starts one publication gap from
/// a freeze.** Delivery to the browser runs at hundreds of Mb/s against a
/// producer that reads a 4K remux off the NAS at not much over 1×, so a
/// client that starts on segment zero drains everything published within
/// seconds and is then advancing at exactly the producer's publication rate.
/// The first time it has to wait a whole publication out, the picture stops
/// for one segment's worth of *production* time — once per film, always at
/// the same second, because where the buffer first runs dry is a property of
/// the film's early bitrate, not of timing. Measured on *Wicked* in Chrome:
/// an 8.8 s freeze at 9.2 s in under a 15 s ceiling, a 5.5 s freeze at 6.0 s
/// under a 6 s one — the freeze moved with the ceiling and scaled with it,
/// which is the experiment that showed shrinking segments approaches this
/// asymptotically and can never remove it. After that first stall the
/// producer's accumulated lead covers every later gap, which is why it
/// happens exactly once.
///
/// So the fix is a head start rather than a segment size: hold the playlist
/// until this many seconds of media exist, and the client begins that far
/// behind the edge. The cushion lives on the server — the client's
/// quota-bounded buffer stays as small as `bufferTargets` says, it just
/// always has published-but-unfetched segments in front of it, and every
/// segment after the gate only adds lead.
///
/// **The unit is seconds, not segments, and it was segments for a day.** A
/// count gate multiplies by whatever the opening happens to cut, and a quiet
/// opening — studio logos run ~30 Mb/s against this film's 61 — is exactly
/// where the 15 s duration ceiling binds: three segments meant
/// 2 + 15 + 15 = 32 s of cushion, twice what the freeze needs, every extra
/// second of it produced at the paced rate before a viewer sees frame one.
/// Measured on *Tron* the day the gate shipped: 21.0 s to first frame, ~16 s
/// of it producing that cushion at exactly 2× pacing. A duration gate opens
/// at the first cut past this line instead (realized cushion: this value up
/// to this value plus one ceiling), which took the same open to
/// 2 + 15 = 17 s of cushion.
///
/// Why 12 s clears the worst gap: the gap is one segment's production time,
/// and duration and production speed are inversely coupled through bitrate.
/// A quiet stretch cuts long segments (up to 15 s) but reads cheap —
/// `HLS_READRATE` holds production at 2×, so the gap is ≤ 7.5 s. A heavy
/// stretch can be NAS-bound near 1×, but its segments are byte-capped: at
/// any bitrate the NAS can sustain playback of at all, a 64 MB segment
/// produces in ≤ ~8.5 s (8.8 s is the worst ever measured, on a 15 s
/// segment under the old regime). 12 covers both shapes; raising it buys
/// margin at one-for-one cost in time-to-first-frame on every paced open.
///
/// The remaining startup cost is honest and has one more owner: an ffmpeg
/// without `-readrate_initial_burst` (pre-6.1 — jellyfin-ffmpeg7 has it)
/// fills the gate at the flat paced rate instead of at I/O speed, which
/// `pacing_caps` now warns about at startup. With the burst working, the
/// cushion is produced as fast as the NAS will go and the gate costs a few
/// seconds; without it, the gate costs cushion ÷ readrate. `Manager::playlist`
/// holds the HTTP response while the gate fills rather than 404ing (verified
/// against the vendored hls.js: the manifest loader waits indefinitely for
/// the first byte — `maxTimeToFirstByteMs` is `Infinity` — inside a 20 s
/// per-attempt window with two retries).
///
/// At end of stream the gate yields: a film shorter than the cushion
/// publishes whole, `ENDLIST` and all, the moment it finishes.
pub const COPY_PUBLISH_GATE_SECS: u32 = 12;

/// The bitstream filter every copied HEVC stream gets before a client sees it.
///
/// Two kinds of NAL unit ride inside a `-c:v copy` that have no business on
/// this wire. In-band parameter sets (VPS/SPS/PPS, types 32–34): a
/// Blu-ray-lineage stream repeats all three at every IRAP and copy preserves
/// them, but the `hvc1` sample entry tagged just above *promises they are not
/// there* (ISO 14496-15 — parameter sets live in the sample entry only, which
/// ffmpeg's hvcC already carries). So every fragment used to open with a spec
/// violation, at exactly the boundary cadence of the 4K stutter this exists
/// to fix (docs/STUTTER-4K.md §5.0). And Dolby Vision's EL/RPU units (62/63):
/// tagging `hvc1` rather than `dvh1`/`dvhe` already forecloses any client
/// engaging DV on this path, so those units are dead weight in every access
/// unit — the browser's parser steps over them, and the base layer is
/// HDR10-compatible on its own.
///
/// Removing them is not a transcode. Verified 2026-07-29 on a repeat-headers
/// open-GOP fixture: `framemd5` of the decoded output is bit-identical with
/// and without, HDR10 static metadata (SEI, type 39) survives, and hvcC in
/// the init segment is untouched. A measured side effect worth having:
/// routing copy through a bitstream filter makes ffmpeg re-derive packet
/// timing through the parser, so sample durations land exactly on the frame
/// grid (1001/24000 at 23.976) instead of carrying the MKV's millisecond
/// rounding — which also zeroes the sub-frame presentation overlap the plain
/// copy left at every segment join.
///
/// One more thing a Dolby Vision source has to shed, learned from Safari on
/// an M3 Max (docs/STUTTER-4K.md): the *container signaling*. Stripping the
/// RPU/EL NAL units leaves the stream's DOVI configuration side data intact,
/// and the muxer writes it out as a `dvcC` box — a stream that DECLARES
/// Dolby Vision Profile 7 (the dual-layer Blu-ray profile no browser and no
/// Apple device supports) while carrying none of its data. Chrome ignores
/// the box; VideoToolbox honours it, and Safari answered with a software
/// decode of 4K10 HEVC on hardware that has a dedicated HEVC block. The
/// `dovi_rpu=strip=1` filter (ffmpeg ≥ 7.1) removes the RPUs *and* the DOVI
/// side data, so nothing is left to write a `dvcC` from and the stream is
/// signalled as what it now is: plain HDR10.
pub fn hevc_copy_bsf(hdr: Option<&str>, have_dovi_bsf: bool) -> String {
    if hdr == Some("dolby_vision") {
        if have_dovi_bsf {
            "dovi_rpu=strip=1,filter_units=remove_types=32-34|62-63".to_owned()
        } else {
            "filter_units=remove_types=32-34|62-63".to_owned()
        }
    } else {
        "filter_units=remove_types=32-34".to_owned()
    }
}

/// Whether this ffmpeg has the `dovi_rpu` bitstream filter (landed in 7.1).
///
/// Parsed from the version line the daemon already collects at boot
/// ("ffmpeg version 7.1.4-Jellyfin …"). Unknown parses answer `false`: the
/// fallback path merely leaves the `dvcC` box behind, while passing an
/// unknown filter to an older ffmpeg is a hard exit for the whole session.
pub fn ffmpeg_has_dovi_bsf(version_line: &str) -> bool {
    // The first token carrying a digit is the version, wherever the build put
    // it: "7.1.4-Jellyfin", "n7.1.2", "6.1.1-3ubuntu5" all qualify.
    let v = version_line
        .split_whitespace()
        .find(|w| w.chars().any(|c| c.is_ascii_digit()))
        .unwrap_or("");
    let mut parts = v.split(|c: char| !c.is_ascii_digit()).filter_map(|p| {
        if p.is_empty() {
            None
        } else {
            p.parse::<u32>().ok()
        }
    });
    match (parts.next(), parts.next()) {
        (Some(major), _) if major > 7 => true,
        (Some(7), Some(minor)) => minor >= 1,
        (Some(7), None) => false,
        _ => false,
    }
}

/// How fast ffmpeg may read a session's input, as data.
///
/// The daemon resolves this — it is the half that knows what the ffmpeg build
/// supports (`-readrate` landed in 5.1, `-readrate_initial_burst` in 6.1, and
/// passing either to an older build is a hard exit rather than a warning) and
/// what the admin configured. The builders here stay pure functions of it.
///
/// Why pacing exists at all: an unpaced `-c copy` is a disk-to-socket pipe
/// that will write a whole 4K film into the session directory as fast as the
/// NAS will serve it. Why it is not simply `1x`: a session paced at exactly
/// realtime can never build a buffer, so the player's runway is whatever it
/// managed to fetch before playback started and every jitter after that is a
/// visible stall. The shape that satisfies both is burst-then-hold — fill a
/// comfortable buffer immediately, then settle to a small multiple of realtime
/// and let the ahead-window suspend (`TranscodeManager`) bound the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pacing {
    /// Multiple of realtime to read the input at. `None` leaves it unpaced.
    pub readrate: Option<f64>,
    /// Seconds delivered flat-out before the rate engages. `None` omits the
    /// clause (an ffmpeg between 5.1 and 6.1 has the rate but not the burst).
    pub initial_burst: Option<f64>,
    /// Use bare `-re` instead: exactly 1x, no burst, and the only pacing an
    /// ffmpeg older than 5.1 understands. The degradation path, not a choice.
    pub legacy_re: bool,
}

impl Pacing {
    /// No pacing at all — the input is read as fast as it can be.
    pub fn unpaced() -> Pacing {
        Pacing::default()
    }

    /// The flags for **one** input. Must be placed before that input's `-i`,
    /// like `-ss`: these are input options, and a second input left unpaced
    /// drags the whole pipeline back to flat-out because the muxer interleaves
    /// them.
    ///
    /// Public because the progressive remux (`/stream.mp4`) builds a
    /// `Command` rather than an argument vector and needs the same flags —
    /// one place decides their shape, so the two delivery paths cannot drift.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        self.push(&mut args);
        args
    }

    fn push(&self, args: &mut Vec<String>) {
        if self.legacy_re {
            args.push("-re".into());
            return;
        }
        let Some(rate) = self.readrate.filter(|r| *r > 0.0) else {
            return;
        };
        if let Some(burst) = self.initial_burst.filter(|b| *b > 0.0) {
            args.push("-readrate_initial_burst".into());
            args.push(format!("{burst:.1}"));
        }
        args.push("-readrate".into());
        args.push(format!("{rate:.2}"));
    }
}

/// How HDR→SDR tone-mapping is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMap {
    /// CPU zscale+tonemap — always available, no GPU needed (the default).
    Zscale,
    /// libplacebo (Vulkan) — higher quality on a capable GPU; opt-in.
    Libplacebo,
    /// No tone-mapping: pass HDR through to 8-bit (looks washed on an SDR
    /// screen, but plays). An escape hatch + a diagnostic — if a file that
    /// grayed out now plays, the tone-map was the culprit.
    None,
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
    /// The video path this session should use. Chosen per node by probe (see
    /// [`Pipeline`]); [`Pipeline::Cpu`] is the always-available default and
    /// what a GPU graph falls back to when it fails at runtime.
    pub pipeline: Pipeline,
    pub subtitle_burn: Option<SubtitleBurn>,
    /// Pass the encoder's forced-IDR option, so `-force_key_frames` produces
    /// key frames the HLS muxer can actually cut at (see
    /// [`Encoder::forced_idr_flag`]). Set from the startup probe, which is
    /// what establishes that this build accepts it.
    ///
    /// Deliberately NOT part of the cache recipe: it changes where segments
    /// begin, not what any frame looks like, and an entry produced before this
    /// existed decodes to the same picture.
    pub force_idr: bool,
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
            pipeline: Pipeline::Cpu,
            subtitle_burn: None,
            force_idr: false,
        }
    }
}

/// Label the burned video carries out of `-filter_complex`.
const BURNED_VIDEO_LABEL: &str = "[vout]";

/// The exact frame size a session will produce: the target height, never above
/// the source's, with an even width at the source's aspect.
///
/// Worth computing rather than leaving to `scale=-2:...` because a bitmap
/// subtitle has to be scaled to *match* it, and "whatever the scaler decided"
/// is not something a second filter can be told. `None` when the source was
/// never probed — an unprobed file has no aspect to preserve, and guessing one
/// would misshape the picture rather than the subtitle.
pub fn output_size(source: &MediaFile, target_height: i64) -> Option<(i64, i64)> {
    let (sw, sh) = (source.width?, source.height?);
    if sw <= 0 || sh <= 0 {
        return None;
    }
    let h = target_height.min(sh).max(2);
    // Even dimensions: yuv420p has half-resolution chroma, so an odd side is
    // not representable and every encoder rejects it.
    let w = ((sw * h + sh / 2) / sh).max(2);
    Some((w & !1, h & !1))
}

/// The `-filter_complex` fragment that renders a bitmap subtitle to the size
/// of the output frame, ready to composite. `None` when nothing is burned, the
/// burn is text (which the chain handles inline), or the source was never
/// probed.
///
/// The scaling is the whole subtlety, and getting it wrong is not subtle at
/// all. A PGS stream carries its own canvas size and positions its bitmaps
/// against that canvas — and on a UHD Blu-ray the canvas is very often 1920×1080
/// while the video is 3840×2160. Composited as-is, every subtitle lands at
/// quarter size in the upper-left quadrant. Scaling the subtitle plane to the
/// output frame first puts it back where it was authored, whichever way the
/// two sizes differ.
fn bitmap_overlay(source: &MediaFile, opts: &TranscodeOptions) -> Option<String> {
    let burn = opts.subtitle_burn.as_ref().filter(|b| b.bitmap)?;
    let (w, h) = output_size(source, opts.target_height)?;
    Some(format!(
        "[0:s:{idx}]scale={w}:{h}[sburn]",
        idx = burn.subtitle_index
    ))
}

/// Build the video filter chain: scale (never upscale) → tone-map (if the
/// source is HDR) → subtitle burn-in. Returns `None` when no filtering is
/// needed (rare for transcode, but keeps the caller simple).
fn video_filters(source: &MediaFile, opts: &TranscodeOptions, source_path: &str) -> String {
    let mut chain: Vec<String> = Vec::new();

    // A GPU pipeline owns scale and tone-map together: they are one pass on
    // the video-processing block, and splitting them would put the download
    // this exists to remove back in the middle. It is only ever selected for
    // sessions it can handle — `effective_pipeline` has already routed HLG,
    // Dolby Vision and burned text subtitles to the CPU chain.
    if let Some(gpu) = opts
        .pipeline
        .filters(opts.target_height, source.hdr.as_deref())
    {
        chain.push(gpu);
        return with_subtitles(chain, opts, source_path);
    }

    // Downscale to target height, keep aspect, even dims, never upscale.
    chain.push(format!("scale=-2:'min({h},ih)'", h = opts.target_height));

    // HDR → SDR tone-map when the source carries HDR.
    if source.hdr.is_some() && opts.tone_map != ToneMap::None {
        match opts.tone_map {
            ToneMap::None => {} // guarded out above; format normalize happens below
            ToneMap::Libplacebo => chain.push(
                "libplacebo=tonemapping=bt.2390:colorspace=bt709:color_primaries=bt709:\
                 color_trc=bt709:format=yuv420p"
                    .to_owned(),
            ),
            ToneMap::Zscale => {
                // Declare the input transfer/primaries/matrix explicitly instead
                // of letting zscale read them off the frame. Hardware decode
                // (QSV/VAAPI) frequently drops color metadata across the
                // hwdownload, so an inferred `t=linear` mis-maps the PQ signal to
                // a flat gray picture — the exact 4K-HDR/DV symptom. HDR is
                // BT.2020; PQ (HDR10/HDR10+/DV) vs HLG differ only in transfer.
                let tin = if source.hdr.as_deref() == Some("hlg") {
                    "arib-std-b67"
                } else {
                    "smpte2084"
                };
                chain.push(format!(
                    "zscale=tin={tin}:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,\
                     tonemap=tonemap=hable:desat=0,\
                     zscale=p=bt709:t=bt709:m=bt709:r=tv,format=yuv420p"
                ));
            }
        }
    } else {
        // Normalize to a browser-safe pixel format.
        chain.push("format=yuv420p".to_owned());
    }

    with_subtitles(chain, opts, source_path)
}

/// Subtitle burn-in, last, so subs render at output resolution in the output
/// color space. Text/ASS is rendered with libass (covers the styled
/// anime-subtitle case, REQ-SUB-2). Bitmap subs are not here at all: they are
/// a second stream composited over this chain's output by [`bitmap_overlay`],
/// which `-vf` cannot express.
///
/// Shared by both branches above because it is genuinely the same step: the
/// `subtitles` filter is CPU-only either way, which is why a session that
/// burns text subtitles is routed to the CPU pipeline in the first place.
fn with_subtitles(mut chain: Vec<String>, opts: &TranscodeOptions, source_path: &str) -> String {
    if let Some(burn) = &opts.subtitle_burn {
        // A bitmap burn is not part of this chain: it is a second stream
        // composited over the chain's output, built by `bitmap_overlay`.
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

/// Decode-side flags, plus an optional filter-chain prefix.
///
/// The heavy case — HEVC that is 4K and/or HDR/Dolby Vision — is hardware-decoded
/// on Intel (QSV/VAAPI) as well, not just NVIDIA. Software-decoding a 4K 10-bit
/// HEVC/DV stream pins the CPU near 100% per core (the "~1000% CPU" symptom) and
/// is far too slow to finish even the first HLS segment, so the player sits on a
/// gray screen until the idle reaper kills the session. GPU-decoded frames arrive
/// as hardware surfaces, so a matching `hwdownload` (to the source's pixel format)
/// leads the CPU scale/tonemap chain before the encoder re-uploads them.
///
/// Lighter sources keep software decode: it's the most compatible path and is
/// already fast enough, so we don't risk the GPU decode/filter handoff where it
/// isn't needed.
/// Is this the kind of source that justifies hardware decode — and, with it,
/// a GPU filter graph?
///
/// The distinction is the whole reason light sources still take the software
/// path: the GPU decode/filter handoff is a real risk (driver bugs, surface
/// format mismatches, Dolby Vision profiles that decode to garbage), and it is
/// worth taking only where the CPU cannot cope. A 1080p H.264 scale is not a
/// problem anyone has. Software-decoding 4K 10-bit HEVC pins a core at 100%
/// and cannot finish the first segment, which is a gray screen.
///
/// Shared with [`Pipeline::for_session`] so the decode choice and the filter
/// choice are made against one definition — a GPU graph selected for a source
/// that decodes in software would be a graph with nothing on the GPU to work
/// on.
pub fn heavy_source(source: &MediaFile) -> bool {
    matches!(
        source.video_codec.as_deref(),
        Some("hevc" | "h265" | "hevc10")
    ) && (source.hdr.is_some() || source.height.unwrap_or(0) >= 2160)
}

fn decode_setup(encoder: Encoder, source: &MediaFile) -> (Vec<String>, Option<String>) {
    let arg = |x: &str| x.to_owned();
    // Escape hatch: force software decode (still hardware-encodes). Set when a
    // GPU decodes a stream to garbage — some Dolby Vision profiles — so you can
    // fall back without giving up the hardware encoder.
    if matches!(
        std::env::var("PLURX_HWDECODE").as_deref(),
        Ok("off" | "0" | "false" | "no")
    ) {
        return (Vec::new(), None);
    }
    let heavy = heavy_source(source);
    // 10-bit (HDR/DV) surfaces download as p010le; 8-bit as nv12.
    let dl = if source.bit_depth.unwrap_or(8) >= 10 {
        "p010le"
    } else {
        "nv12"
    };
    match encoder {
        // These families decode into system memory implicitly — no hwdownload.
        Encoder::Nvenc => (vec![arg("-hwaccel"), arg("cuda")], None),
        Encoder::VideoToolbox => (vec![arg("-hwaccel"), arg("videotoolbox")], None),
        // Intel: keep frames on the GPU through decode, then hwdownload for the
        // CPU filters. Only for the heavy case that actually needs it.
        Encoder::Qsv if heavy => (
            vec![
                arg("-hwaccel"),
                arg("qsv"),
                arg("-hwaccel_output_format"),
                arg("qsv"),
            ],
            Some(format!("hwdownload,format={dl}")),
        ),
        Encoder::Vaapi if heavy => (
            vec![
                arg("-hwaccel"),
                arg("vaapi"),
                arg("-hwaccel_output_format"),
                arg("vaapi"),
            ],
            Some(format!("hwdownload,format={dl}")),
        ),
        // Software, and the light QSV/VAAPI cases: software decode.
        _ => (Vec::new(), None),
    }
}

/// Build the full ffmpeg argument vector to transcode `source` into HLS in
/// `out_dir` (which must exist). Produces `index.m3u8` + `seg%05d.ts`.
pub fn hls_args(
    source: &MediaFile,
    encoder: Encoder,
    opts: &TranscodeOptions,
    pacing: Pacing,
    out_dir: &str,
) -> Vec<String> {
    let source_path = source.path.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];

    // Hardware device init (VAAPI/QSV) must precede the input, and so must a
    // filter device the pipeline brings of its own (Vulkan for libplacebo,
    // OpenCL for tonemap_opencl).
    args.extend(encoder.init_args());
    args.extend(opts.pipeline.init_args());

    // Fast input seek for resume/session start.
    if opts.start_seconds > 0.0 {
        args.push("-ss".into());
        args.push(format!("{:.3}", opts.start_seconds));
    }

    // Hardware-accelerated decode (GPU decode for the heavy HEVC/4K/HDR case on
    // Intel too; see decode_setup). `hwdownload` leads the filter chain when the
    // decoder hands back hardware surfaces.
    // A vendor GPU pipeline claims the decoder itself — it needs hardware
    // surfaces of its own family to work on, and cannot start from frames that
    // were never on the GPU. Everything else keeps `decode_setup`'s answer,
    // which knows things the pipeline doesn't (source codec, bit depth, and
    // the `PLURX_HWDECODE=off` escape hatch for Dolby Vision profiles that
    // hardware-decode to garbage).
    let pipeline_decode = opts.pipeline.decode_args();
    let (decode_args, hwdownload) = if pipeline_decode.is_empty() {
        decode_setup(encoder, source)
    } else {
        (pipeline_decode, None)
    };
    args.extend(decode_args);

    // Pace this input (see [`Pacing`]). An encode that runs faster than
    // realtime otherwise writes the whole film ahead of a playhead that will
    // never reach most of it.
    pacing.push(&mut args);
    args.push("-i".into());
    args.push(source_path.clone());

    // A per-file A/V sync correction rides in on a second input of the same
    // file, shifted with -itsoffset and used only for its audio (positive =
    // audio later). Both inputs get the same fast input-seek so resume stays
    // aligned; the video still comes from input 0 (with its hw decode).
    let audio_input = if source.audio_offset_ms != 0 {
        if opts.start_seconds > 0.0 {
            args.push("-ss".into());
            args.push(format!("{:.3}", opts.start_seconds));
        }
        pacing.push(&mut args);
        args.push("-itsoffset".into());
        args.push(format!("{:.3}", source.audio_offset_ms as f64 / 1000.0));
        args.push("-i".into());
        args.push(source_path.clone());
        1
    } else {
        0
    };

    // Video filter chain: [hwdownload for GPU-decoded frames →] scale / tonemap /
    // subs [→ GPU upload suffix for VAAPI/QSV] → encoder.
    //
    // A vendor GPU pipeline skips both ends. Its output is already a surface
    // of its encoder's own family, so the upload suffix would be uploading
    // something that never came down — which is not a wasted copy but a broken
    // graph. The neutral pipelines download explicitly inside their own chain
    // and then take the suffix like the CPU path does.
    let vendor_gpu = matches!(opts.pipeline, Pipeline::VppQsv | Pipeline::TonemapVaapi);
    let mut vf = String::new();
    if let Some(prefix) = &hwdownload {
        vf.push_str(prefix);
        vf.push(',');
    }
    vf.push_str(&video_filters(source, opts, &source_path));
    if let Some(suffix) = encoder.filter_suffix().filter(|_| !vendor_gpu) {
        vf.push(',');
        vf.push_str(suffix);
    }

    // A bitmap subtitle is a picture, so burning one is a *composite* — two
    // streams into one filter — and that needs `-filter_complex` and a mapped
    // output label rather than the single-stream `-vf`. Everything above stays
    // the chain it was; this only wraps it.
    let overlay = bitmap_overlay(source, opts);
    // Map the chosen (or default) audio, and video from wherever it now comes.
    args.push("-map".into());
    args.push(match &overlay {
        Some(_) => BURNED_VIDEO_LABEL.to_owned(),
        None => "0:v:0".to_owned(),
    });
    args.push("-map".into());
    match opts.audio_index {
        Some(i) => args.push(format!("{audio_input}:a:{i}?")),
        None => args.push(format!("{audio_input}:a:0?")),
    }

    match overlay {
        Some(sub) => {
            args.push("-filter_complex".into());
            args.push(format!(
                "[0:v]{vf}[vburn];{sub};\
                 [vburn][sburn]overlay=eof_action=pass{BURNED_VIDEO_LABEL}"
            ));
        }
        None => {
            args.push("-vf".into());
            args.push(vf);
        }
    }
    args.extend(encoder.encode_args(opts.video_bitrate_kbps, opts.force_idr));

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

/// Everything a copy session's ffmpeg does *before* the muxer: input seek,
/// pacing, stream mapping, the video copy with its bitstream hygiene, and the
/// audio decision.
///
/// Shared by [`hls_copy_args`] (ffmpeg's HLS muxer writes the segments) and
/// [`copy_pipe_args`] (plurx cuts the segments itself, `crate::fmp4`). The two
/// paths deliver the same bitstream by different routes, and every argument up
/// to the muxer has to be identical for that to be true — a second copy of
/// this list is a divergence waiting for the day one of them is edited.
fn copy_input_args(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
) -> Vec<String> {
    let source_path = source.path.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];

    // Fast input seek for resume/session start.
    if start_seconds > 0.0 {
        args.push("-ss".into());
        args.push(format!("{start_seconds:.3}"));
    }
    // Copy runs as fast as the disk allows, so it has to be paced — but *not*
    // at 1x, which is what a bare `-re` here used to do. Realtime pacing meant
    // the segments existed exactly as fast as they were consumed, so a 4K
    // session's runway was whatever the player fetched before it started and
    // never grew: the "starts fine, buffers ten seconds in" report, and the
    // reason an Apple TV (which wants ~3 segments before it plays) took a
    // dozen seconds to start. The disk is bounded by the ahead-window suspend
    // and the behind-playhead GC now, not by starving the viewer.
    pacing.push(&mut args);
    args.push("-i".into());
    args.push(source_path.clone());

    // A per-file A/V sync correction rides in on a second, `-itsoffset`'d input
    // of the same file, used only for its audio (positive = audio later).
    let audio_input = if source.audio_offset_ms != 0 {
        if start_seconds > 0.0 {
            args.push("-ss".into());
            args.push(format!("{start_seconds:.3}"));
        }
        pacing.push(&mut args);
        args.push("-itsoffset".into());
        args.push(format!("{:.3}", source.audio_offset_ms as f64 / 1000.0));
        args.push("-i".into());
        args.push(source_path.clone());
        1
    } else {
        0
    };

    // Copy the video untouched; map the chosen (or first) audio; drop subs.
    //
    // And drop CHAPTERS, which `-map` does not govern. ffmpeg's mp4 muxer
    // writes a source's chapters as a QuickTime `text` track plus a `chpl`
    // box, with `tref` links from the real tracks — a third track, in the
    // init and in every fragment, that nothing asked for. ffmpeg's own HLS
    // muxer never carried it, so the copy path never had it; the segmenter
    // reads a plain `-f mp4` pipe, which does. Safari refused the resulting
    // stream outright (`MEDIA_ERR_DECODE`, 708 ms in) and the player's error
    // fallback re-encoded a 4K remux down to 1080p. Chrome ignored the extra
    // track entirely, which is why only real hardware found it — and every
    // disc remux has chapters while no synthetic fixture does.
    //
    // Nothing is lost: plurx serves chapter markers from ffprobe at playback
    // start (ARCHITECTURE §7 decision 6), never from the media stream.
    args.push("-map_chapters".into());
    args.push("-1".into());
    args.push("-map".into());
    args.push("0:v:0".into());
    args.push("-map".into());
    match audio_index {
        Some(i) => args.push(format!("{audio_input}:a:{i}?")),
        None => args.push(format!("{audio_input}:a:0?")),
    }
    args.push("-sn".into());

    args.push("-c:v".into());
    args.push("copy".into());
    // Safari only decodes HEVC when the sample entry is tagged `hvc1`; MKV HEVC
    // is commonly `hev1`, which renders black. Harmless if already hvc1.
    if matches!(source.video_codec.as_deref(), Some("hevc" | "h265")) {
        args.push("-tag:v".into());
        args.push("hvc1".into());
        args.push("-bsf:v".into());
        args.push(hevc_copy_bsf(source.hdr.as_deref(), have_dovi_bsf));
    }

    if transcode_audio {
        // e.g. DTS/TrueHD → AAC. Keep the source channel layout (5.1 stays 5.1);
        // a copy-video session implies a player that can take multichannel AAC.
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        args.push("256k".into());
    } else {
        args.push("-c:a".into());
        args.push("copy".into());
    }
    args
}

/// Build ffmpeg args for a copy session that plurx segments itself.
///
/// One continuous fragmented MP4 down stdout — `frag_keyframe` puts one
/// fragment per GOP on the wire, `delay_moov` lets the `moov` describe the
/// real stream instead of a guess, and `default_base_moof` keeps every sample
/// offset relative to its own `moof` so a reader that has never seen the file
/// start can resolve them. `crates/plurx-core/src/fmp4.rs` reads that stream
/// and decides where the segment boundaries go; the point of moving the cut
/// out of ffmpeg is that a boundary can then be placed only in front of a
/// keyframe no player will discard a leading picture at.
///
/// `-avoid_negative_ts make_zero` comes from the progressive remux, for its
/// reasons: an input seek leaves the first packet's timestamp wherever the
/// keyframe was, and a fragmented MP4 whose timeline does not start at zero is
/// a stream players disagree about.
pub fn copy_pipe_args(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
) -> Vec<String> {
    let mut args = copy_input_args(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        have_dovi_bsf,
    );
    args.extend(
        [
            "-avoid_negative_ts",
            "make_zero",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+delay_moov",
            "-f",
            "mp4",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

/// Build ffmpeg args to *copy* the source video into HLS (fMP4 segments) with
/// ffmpeg's own HLS muxer doing the cutting, transcoding only the audio when
/// the client can't take the source codec.
///
/// This is "remux, packaged as HLS". Safari's `<video>` will not play a
/// progressive fragmented MP4 (the `/stream.mp4` remux) — it only accepts
/// fragmented content via HLS — but it decodes HEVC/HDR natively through HLS.
/// So for those clients we keep the original 4K video stream untouched and
/// repackage it as HLS, instead of letting the player's error-fallback
/// re-encode the whole thing down to 720p. fMP4 segments (not MPEG-TS) are
/// required: Apple does not support HEVC in a TS container.
///
/// Since the GOP-aware segmenter this is the FALLBACK path — taken for a
/// source whose keyframes `crate::fmp4` cannot read, and for a stream it
/// turned out not to be able to follow. It cuts wherever it finds a keyframe
/// past the floor, which on an open-GOP source costs one frame per boundary
/// (docs/STUTTER-4K.md §5.6). That is the behaviour the segmenter exists to
/// improve on, and the behaviour anything unexpected degrades back to.
pub fn hls_copy_args(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
    out_dir: &str,
) -> Vec<String> {
    let mut args = copy_input_args(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        have_dovi_bsf,
    );

    // fMP4 HLS. Segments split at existing keyframes (copy can't force them), so
    // a normally-GOP'd source segments cleanly; the init segment is `init.mp4`.
    //
    // NO `independent_segments` here, and the omission is the point. That tag
    // declares every segment independently decodable, which is TRUE for the
    // transcode path (closed GOP, forced IDR) and FALSE for a copy of an
    // open-GOP source: each segment opens with a CRA whose RASL leading
    // picture needs the previous segment's frames. A player that believes the
    // tag may treat every segment start as a random-access point, and the
    // HEVC spec's instruction at random access is to DISCARD the leading
    // pictures — one dropped frame per segment boundary, which is exactly
    // the measured residual stutter (22 drops in 45s, 15/23 within 150ms of
    // a boundary, docs/STUTTER-4K.md §5.5). The claim was a lie on this path;
    // whether any given player acts on it, a lie in a spec tag is not a
    // thing to keep shipping.
    args.extend(
        [
            "-f",
            "hls",
            "-hls_time",
            &COPY_SEGMENT_SECONDS.to_string(),
            "-hls_playlist_type",
            "event",
            "-hls_flags",
            "temp_file",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "init.mp4",
            "-hls_segment_filename",
            &format!("{out_dir}/seg%05d.m4s"),
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
            hdr_format: None,
            bitrate: Some(60_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    #[test]
    fn software_hls_args_are_well_formed() {
        let opts = TranscodeOptions {
            target_height: 1080,
            video_bitrate_kbps: 6000,
            ..Default::default()
        };
        let args = hls_args(
            &file(None),
            Encoder::Software,
            &opts,
            Pacing::unpaced(),
            "/tmp/sess",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-i /media/movie.mkv"));
        assert!(joined.contains("libx264"));
        assert!(joined.contains("scale=-2:'min(1080,ih)'"));
        assert!(joined.contains("-f hls"));
        assert!(joined.contains("/tmp/sess/index.m3u8"));
        // The forced-keyframe grid and the muxer's segment length are the same
        // number by construction — a session whose keyframes don't land on its
        // segment boundaries produces segments that are not independently
        // decodable, which is the one thing HLS requires of them. Written
        // against the constant so the two can never be tuned apart.
        assert!(joined.contains(&format!("expr:gte(t,n_forced*{SEGMENT_SECONDS})")));
        assert!(joined.contains(&format!("-hls_time {SEGMENT_SECONDS}")));
        // SDR source → no tonemap, just pixel-format normalize.
        assert!(joined.contains("format=yuv420p"));
        assert!(!joined.contains("tonemap"));
    }

    /// The geometry that a naive overlay gets wrong, and gets wrong invisibly
    /// to anything but an eye: a UHD Blu-ray very often carries a 1920×1080
    /// PGS canvas over 3840×2160 video, and compositing that as-is puts every
    /// subtitle at quarter size in the upper-left quadrant. Scaling the
    /// subtitle plane to the *output* frame is what puts it back.
    #[test]
    fn a_bitmap_burn_scales_the_subtitle_to_the_output_frame() {
        let mut f = file(None);
        f.width = Some(3840);
        f.height = Some(2160);
        let opts = TranscodeOptions {
            target_height: 1080,
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 2,
                bitmap: true,
            }),
            ..Default::default()
        };
        let args = hls_args(&f, Encoder::Software, &opts, Pacing::unpaced(), "/tmp/s");
        let joined = args.join(" ");

        // Two streams into one filter needs -filter_complex and a mapped label;
        // -vf cannot express it at all.
        assert!(joined.contains("-filter_complex"), "{joined}");
        assert!(!joined.contains("-vf "), "a burn must not also use -vf");
        assert!(
            joined.contains("-map [vout]"),
            "the burned video is what plays"
        );
        assert!(
            !joined.contains("-map 0:v:0"),
            "mapping the raw video skips the burn"
        );

        // The subtitle is scaled to the frame the viewer will see, not to the
        // source and not to its own canvas.
        assert!(joined.contains("[0:s:2]scale=1920:1080[sburn]"), "{joined}");
        // `eof_action=pass` rather than the default `repeat`: a subtitle
        // stream ends before the film does, and repeating its last frame both
        // freezes a subtitle on screen for the rest of the runtime and makes
        // the encoder log an alarming duplication error about a gap that is
        // not a problem. Verified against a real PGS stream.
        assert!(
            joined.contains("[vburn][sburn]overlay=eof_action=pass[vout]"),
            "{joined}"
        );

        // A text burn is a filter in the chain, not a composite — it must not
        // take this path.
        let text = TranscodeOptions {
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 0,
                bitmap: false,
            }),
            ..opts.clone()
        };
        let joined = hls_args(&f, Encoder::Software, &text, Pacing::unpaced(), "/tmp/s").join(" ");
        assert!(joined.contains("-vf "), "text burn stays a simple chain");
        assert!(joined.contains("subtitles="));
        assert!(!joined.contains("overlay"));
    }

    /// The output frame is what a bitmap subtitle is scaled against, so its
    /// arithmetic has to be exactly the scaler's: never upscale, keep the
    /// aspect, and land on even sides.
    #[test]
    fn the_output_frame_matches_what_the_scaler_would_produce() {
        let mut f = file(None);
        f.width = Some(3840);
        f.height = Some(2160);
        assert_eq!(output_size(&f, 1080), Some((1920, 1080)));
        assert_eq!(output_size(&f, 720), Some((1280, 720)));
        // Never upscales: asking for more than the source has yields the source.
        assert_eq!(output_size(&f, 2160), Some((3840, 2160)));
        assert_eq!(output_size(&f, 4320), Some((3840, 2160)));

        // Odd arithmetic rounds to even — yuv420p cannot represent an odd side,
        // and every encoder refuses one.
        f.width = Some(1919);
        f.height = Some(1079);
        let (w, h) = output_size(&f, 720).expect("size");
        assert_eq!((w % 2, h % 2), (0, 0), "got {w}x{h}");

        // A file the probe never read has no aspect to preserve. Guessing one
        // would misshape the picture, so nothing is claimed.
        f.width = None;
        assert_eq!(output_size(&f, 1080), None);
    }

    #[test]
    fn hdr_source_inserts_tonemap() {
        let args = hls_args(
            &file(Some("hdr10")),
            Encoder::Software,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("tonemap=tonemap=hable"));
        assert!(joined.contains("zscale"));
    }

    #[test]
    fn tonemap_none_passes_hdr_through() {
        // PLURX_TONEMAP=off → no tone-map filter, HDR just normalized to yuv420p.
        let opts = TranscodeOptions {
            tone_map: ToneMap::None,
            ..Default::default()
        };
        let args = hls_args(
            &file(Some("dolby_vision")),
            Encoder::Software,
            &opts,
            Pacing::unpaced(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(!joined.contains("tonemap"));
        assert!(!joined.contains("zscale"));
        assert!(joined.contains("format=yuv420p"));
    }

    #[test]
    fn start_offset_seeks_input() {
        let opts = TranscodeOptions {
            start_seconds: 90.5,
            ..Default::default()
        };
        let args = hls_args(
            &file(None),
            Encoder::Software,
            &opts,
            Pacing::unpaced(),
            "/tmp/s",
        );
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
        let args = hls_args(
            &file(None),
            Encoder::Software,
            &opts,
            Pacing::unpaced(),
            "/tmp/s",
        );
        assert!(args.join(" ").contains("subtitles='/media/movie.mkv':si=2"));
    }

    #[test]
    fn copy_args_keep_video_and_package_fmp4_hls() {
        // DTS → AAC, HEVC video copied, delivered as fMP4 HLS (the Safari path).
        let args = hls_copy_args(
            &file(Some("hdr10")),
            0.0,
            None,
            true,
            Pacing::unpaced(),
            false,
            "/tmp/sess",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-tag:v hvc1")); // HEVC needs hvc1 for Safari
        assert!(joined.contains("-c:a aac")); // audio transcoded when asked
        assert!(joined.contains("-hls_segment_type fmp4")); // not mpegts
        assert!(joined.contains("-hls_fmp4_init_filename init.mp4"));
        // An open-GOP copy cannot promise independently decodable segments,
        // and a player that believes the promise discards one leading picture
        // per boundary. The transcode path (closed GOP) keeps the tag.
        assert!(!joined.contains("independent_segments"));
        assert!(joined.contains("-hls_flags temp_file"));
        // Every boundary costs a leading picture, so the copy path asks for
        // long segments; the burst covers the first-segment wait. The
        // transcode path keeps SEGMENT_SECONDS — TTFF at 1x encode.
        assert!(joined.contains(&format!("-hls_time {COPY_SEGMENT_SECONDS}")));
        assert!(!joined.contains(&format!("-hls_time {SEGMENT_SECONDS} ")));
        assert!(joined.contains("/tmp/sess/seg%05d.m4s"));
        assert!(joined.contains("/tmp/sess/index.m3u8"));
        // No re-encode: none of the transcode machinery leaks in.
        assert!(!joined.contains("libx264"));
        assert!(!joined.contains("scale="));
        assert!(!joined.contains("tonemap"));
    }

    /// The `hvc1` tag promises no in-band parameter sets, and a
    /// Blu-ray-lineage copy breaks that promise at every IRAP — which on this
    /// path is every segment boundary. The filter is what keeps the promise;
    /// a Dolby Vision source additionally sheds the EL/RPU units nothing on
    /// an hvc1 wire can use.
    #[test]
    fn copied_hevc_keeps_the_hvc1_promise() {
        let joined = hls_copy_args(
            &file(Some("hdr10")),
            0.0,
            None,
            true,
            Pacing::unpaced(),
            false,
            "/tmp/s",
        )
        .join(" ");
        assert!(joined.contains("-bsf:v filter_units=remove_types=32-34"));
        assert!(!joined.contains("62-63"), "HDR10 keeps nothing to shed");

        let dv = hls_copy_args(
            &file(Some("dolby_vision")),
            0.0,
            None,
            true,
            Pacing::unpaced(),
            false,
            "/tmp/s",
        )
        .join(" ");
        assert!(dv.contains("-bsf:v filter_units=remove_types=32-34|62-63"));

        // With a capable ffmpeg the DV strip also removes the container
        // signaling: dovi_rpu drops the DOVI side data, so the muxer writes
        // no dvcC and the stream stops claiming a profile nothing supports.
        let dv7 = hls_copy_args(
            &file(Some("dolby_vision")),
            0.0,
            None,
            true,
            Pacing::unpaced(),
            true,
            "/tmp/s",
        )
        .join(" ");
        assert!(dv7.contains("-bsf:v dovi_rpu=strip=1,filter_units=remove_types=32-34|62-63"));

        // H.264 copies are untouched: they are not on the stuttering path and
        // avc1 + in-band parameter sets plays everywhere today.
        let mut f = file(None);
        f.video_codec = Some("h264".into());
        let h264 = hls_copy_args(&f, 0.0, None, true, Pacing::unpaced(), true, "/tmp/s").join(" ");
        assert!(!h264.contains("-bsf:v"));
        assert!(!h264.contains("-tag:v"));
    }

    /// The gate on dovi_rpu is an ffmpeg version parse, and getting it wrong
    /// in either direction has a cost: too eager is a hard session exit on an
    /// older build ("Unknown bit stream filter"), too shy leaves the dvcC box
    /// that made Safari software-decode 4K on an M3 Max.
    #[test]
    fn dovi_bsf_gate_reads_real_version_lines() {
        assert!(ffmpeg_has_dovi_bsf(
            "ffmpeg version 7.1.4-Jellyfin Copyright (c) 2000-2026"
        ));
        assert!(ffmpeg_has_dovi_bsf("ffmpeg version 7.1"));
        assert!(ffmpeg_has_dovi_bsf("ffmpeg version 8.0-static"));
        assert!(!ffmpeg_has_dovi_bsf(
            "ffmpeg version 6.1.1-3ubuntu5 Copyright (c) 2000-2023"
        ));
        assert!(!ffmpeg_has_dovi_bsf("ffmpeg version 7.0.2"));
        assert!(!ffmpeg_has_dovi_bsf("ffmpeg version 7"));
        assert!(!ffmpeg_has_dovi_bsf(""));
        assert!(!ffmpeg_has_dovi_bsf("not a version line at all"));
        // The n-prefixed spelling some builds use.
        assert!(ffmpeg_has_dovi_bsf("ffmpeg version n7.1.2"));
    }

    #[test]
    fn copy_args_copy_audio_when_supported() {
        let args = hls_copy_args(
            &file(None),
            30.0,
            Some(1),
            false,
            Pacing::unpaced(),
            false,
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:a copy")); // transcode_audio = false
        assert!(joined.contains("0:a:1?") || joined.contains("a:1?")); // chosen track
                                                                       // -ss before -i for fast input seek.
        let ss = args.iter().position(|a| a == "-ss").expect("has -ss");
        let i = args.iter().position(|a| a == "-i").expect("has -i");
        assert!(ss < i);
    }

    /// Pacing flags are *input* options: they must land before their own `-i`,
    /// and a second input (the A/V-offset case) needs its own copy or the muxer
    /// interleave drags the whole pipeline back to flat-out.
    #[test]
    fn pacing_precedes_every_input() {
        let pacing = Pacing {
            readrate: Some(2.0),
            initial_burst: Some(90.0),
            legacy_re: false,
        };
        let mut f = file(None);
        f.audio_offset_ms = 250; // forces the second input
        let args = hls_copy_args(&f, 0.0, None, false, pacing, false, "/tmp/s");

        let inputs: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-i")
            .map(|(i, _)| i)
            .collect();
        let rates: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-readrate")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(inputs.len(), 2, "offset file remuxes from two inputs");
        assert_eq!(rates.len(), 2, "every input is paced, not just the first");
        for (rate, input) in rates.iter().zip(&inputs) {
            assert!(rate < input, "pacing is an input option, so it leads -i");
        }
        assert!(args.join(" ").contains("-readrate_initial_burst 90.0"));
        // The old realtime pacing is gone: it is what starved the buffer.
        assert!(!args.contains(&"-re".to_owned()));
    }

    #[test]
    fn pacing_degrades_with_the_ffmpeg_build() {
        let out =
            |p: Pacing| hls_copy_args(&file(None), 0.0, None, false, p, false, "/tmp/s").join(" ");
        // Nothing configured (or pacing switched off): no flags at all.
        assert!(!out(Pacing::unpaced()).contains("-readrate"));
        assert!(!out(Pacing::unpaced()).contains("-re "));
        // 5.1–6.0: rate limiting, no burst clause.
        let rate_only = Pacing {
            readrate: Some(3.0),
            initial_burst: None,
            legacy_re: false,
        };
        assert!(out(rate_only).contains("-readrate 3.00"));
        assert!(!out(rate_only).contains("initial_burst"));
        // Pre-5.1: bare `-re`, and never both.
        let legacy = Pacing {
            readrate: Some(2.0),
            initial_burst: Some(90.0),
            legacy_re: true,
        };
        assert!(out(legacy).contains("-re "));
        assert!(!out(legacy).contains("-readrate"));
    }

    /// A transcode session is paced too — the same burst-then-hold shape. An
    /// encoder faster than realtime otherwise races the playhead and writes
    /// segments nobody fetches.
    #[test]
    fn transcode_input_is_paced_too() {
        let pacing = Pacing {
            readrate: Some(2.0),
            initial_burst: Some(90.0),
            legacy_re: false,
        };
        let args = hls_args(
            &file(None),
            Encoder::Software,
            &TranscodeOptions::default(),
            pacing,
            "/tmp/s",
        );
        let rate = args
            .iter()
            .position(|a| a == "-readrate")
            .expect("paced input");
        let input = args.iter().position(|a| a == "-i").expect("has input");
        assert!(rate < input);
    }

    #[test]
    fn nvenc_swaps_encoder_and_adds_decode() {
        let args = hls_args(
            &file(None),
            Encoder::Nvenc,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("h264_nvenc"));
        assert!(joined.contains("cuda")); // hwaccel decode
    }

    #[test]
    fn qsv_hardware_decodes_heavy_hevc() {
        // 4K 10-bit Dolby Vision HEVC (the file() helper) → GPU decode so it
        // isn't CPU-bound, with an hwdownload to feed the CPU tonemap filters.
        let args = hls_args(
            &file(Some("dolby_vision")),
            Encoder::Qsv,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-hwaccel qsv"));
        assert!(joined.contains("-hwaccel_output_format qsv"));
        assert!(joined.contains("hwdownload,format=p010le")); // 10-bit surface
        assert!(joined.contains("h264_qsv")); // still hardware encode
    }

    #[test]
    fn qsv_software_decodes_light_source() {
        // 1080p 8-bit H.264 doesn't need GPU decode — keep the compatible
        // software-decode path, but still hardware-encode.
        let mut f = file(None);
        f.video_codec = Some("h264".into());
        f.width = Some(1920);
        f.height = Some(1080);
        f.bit_depth = Some(8);
        f.hdr = None;
        let args = hls_args(
            &f,
            Encoder::Qsv,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        let joined = args.join(" ");
        assert!(!joined.contains("-hwaccel qsv")); // software decode
        assert!(!joined.contains("hwdownload"));
        assert!(joined.contains("h264_qsv")); // hardware encode
    }
}
