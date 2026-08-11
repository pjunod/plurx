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

pub use encoder::{
    detect_encoders, validate_quality_rate_control, validate_quality_rate_control_yielding,
    EffectiveRateControl, Encoder, EncoderCaps, QualityRateControlValidation, QualityRc, RateMode,
};
pub use pipeline::{Pipeline, CANDIDATES as PIPELINE_CANDIDATES};
pub use recipe::{PipelineDigest, Recipe};

use crate::domain::MediaFile;
use std::path::PathBuf;

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
/// count gate multiplies by whatever the opening happens to cut, and the cut
/// is a property of the title: a stretch quiet enough for the 15 s duration
/// ceiling to bind turns "three segments" into 2 + 15 + 15 = 32 s of
/// cushion, 2.4× what the freeze needs, every extra second of it produced at
/// the paced rate before a viewer sees frame one. A duration gate promises
/// the cushion in the unit the freeze is measured in; its realized size is
/// this value up to this value plus one segment, whatever the title cuts.
/// (*Tron*, the file that exposed the slow start at 21.0 s to first frame,
/// turned out to cut clean 7.5–8.5 s segments — the overlay's own
/// largest-appended figure said so — so its cushion was ~17 s under either
/// unit, and its 21 s decomposes as ~8 s of cushion at the flat 2× pacing
/// below plus a cold NFS open and probe of a Dolby Vision MKV plus fetch
/// and decode; a warm second play started visibly faster with nothing
/// changed. The count was still the wrong unit — it just wasn't *Tron's*
/// biggest cost.)
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
    hevc_copy_bsf_for_client(hdr, have_dovi_bsf, false)
}

/// Client-aware form of [`hevc_copy_bsf`]. A player that explicitly supports
/// this Dolby Vision profile receives its RPU/EL units unchanged; other
/// clients receive only the compatible HDR base.
pub fn hevc_copy_bsf_for_client(
    hdr: Option<&str>,
    have_dovi_bsf: bool,
    preserve_dolby_vision: bool,
) -> String {
    if hdr == Some("dolby_vision") && preserve_dolby_vision {
        "filter_units=remove_types=32-34".to_owned()
    } else if hdr == Some("dolby_vision") {
        if have_dovi_bsf {
            "dovi_rpu=strip=1,filter_units=remove_types=32-34|62-63".to_owned()
        } else {
            "filter_units=remove_types=32-34|62-63".to_owned()
        }
    } else {
        "filter_units=remove_types=32-34".to_owned()
    }
}

/// HEVC sample-entry tag for the copy output.
///
/// Dolby Vision Profiles 8.1 and 8.4 are backward-compatible enhancements of
/// HDR10 and HLG. Apple's HLS/ISOBMFF contract requires those streams to keep
/// the compatible `hvc1` base sample entry; the Dolby Vision profile is
/// advertised separately by `SUPPLEMENTAL-CODECS`. Non-compatible Dolby
/// Vision (such as Profile 5) remains `dvh1` when it is preserved.
pub fn hevc_copy_tag(hdr: Option<&str>, preserve_dolby_vision: bool) -> &'static str {
    hevc_copy_tag_for_format(hdr, None, preserve_dolby_vision)
}

/// Format-aware sample-entry choice for segmented copies, whose source model
/// includes the richer HDR compatibility label.
pub fn hevc_copy_tag_for_format(
    hdr: Option<&str>,
    hdr_format: Option<&str>,
    preserve_dolby_vision: bool,
) -> &'static str {
    let compatible_base = hdr_format.is_some_and(|format| {
        format.contains("HDR10-compatible") || format.contains("HLG-compatible")
    });
    if hdr == Some("dolby_vision") && preserve_dolby_vision && !compatible_base {
        "dvh1"
    } else {
        "hvc1"
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
    /// Validated rate control. Requested settings never reach this struct: a
    /// refused quality mode is represented as VBR before args or identity are
    /// built.
    pub effective_rate_control: EffectiveRateControl,
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
    /// Cached simple-text subtitle sidecar used by libass. `None` means the
    /// subtitle is bitmap, absent, or a styled embedded source retained for
    /// compatibility.
    /// Not part of the recipe: the selected stream identifies the pixels;
    /// this only avoids reopening and scanning the full media file.
    pub subtitle_file: Option<PathBuf>,
    /// Pass the encoder's forced-IDR option, so `-force_key_frames` produces
    /// key frames the HLS muxer can actually cut at (see
    /// [`Encoder::forced_idr_flag`]). Set from the startup probe, which is
    /// what establishes that this build accepts it.
    ///
    /// Deliberately NOT part of the cache recipe: it changes where segments
    /// begin, not what any frame looks like, and an entry produced before this
    /// existed decodes to the same picture.
    pub force_idr: bool,
    /// Explicit thread budget for the software encoder, from the admission
    /// pool's permit (plurxd's `Workload::software_threads`). `None` lets
    /// x264 pick — which is cores x 1.5, a fine answer for exactly one
    /// session and an oversubscription for two. Ignored by hardware
    /// encoders: their parallelism lives in silicon, not in threads.
    ///
    /// Also deliberately NOT part of the cache recipe, for force_idr's
    /// reason: it changes how fast frames are made, not what they look like
    /// to a viewer — and on any one box the same workload computes the same
    /// budget anyway.
    pub software_threads: Option<u32>,
}

/// The AAC bitrate every HLS transcode uses unless asked otherwise — public
/// because the quality ladder's advertised totals must include the audio a
/// session will actually carry, from the same constant rather than a copy.
pub const AUDIO_BITRATE_KBPS_DEFAULT: u32 = 160;

impl Default for TranscodeOptions {
    fn default() -> Self {
        TranscodeOptions {
            target_height: 1080,
            video_bitrate_kbps: 8000,
            effective_rate_control: EffectiveRateControl::Vbr,
            audio_channels: 2,
            audio_bitrate_kbps: AUDIO_BITRATE_KBPS_DEFAULT,
            audio_index: None,
            start_seconds: 0.0,
            tone_map: ToneMap::Zscale,
            pipeline: Pipeline::Cpu,
            subtitle_burn: None,
            subtitle_file: None,
            force_idr: false,
            software_threads: None,
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
    // sessions it can handle — `Pipeline::for_session` has already routed HLG
    // and unsupported Dolby Vision to the CPU chain. Subtitle burns ride the
    // GPU graph too: scale + tone-map happen first, then vendor surfaces come
    // down to system memory exactly once for libass/overlay. What stays off
    // the CPU is the float tone-map — the cost that put a 4K burn under
    // realtime while the GPU graph measured 4.9× (PERF-PLAN §5).
    let bitmap_burn = opts.subtitle_burn.as_ref().is_some_and(|b| b.bitmap);
    let text_burn = opts.subtitle_burn.as_ref().is_some_and(|b| !b.bitmap);
    let burn_size = if bitmap_burn {
        output_size(source, opts.target_height)
    } else {
        None
    };
    if let Some(gpu) = opts.pipeline.filters(
        burn_size.map(|(w, _)| w),
        burn_size.map_or(opts.target_height, |(_, h)| h),
        source.hdr.as_deref(),
    ) {
        if (bitmap_burn || text_burn)
            && matches!(opts.pipeline, Pipeline::VppQsv | Pipeline::TonemapVaapi)
        {
            // These two end in vendor surfaces (their encoders read them
            // directly); the composite cannot. Libplacebo and OpenCL already
            // finish with their own download, so they need nothing here.
            chain.push(format!("{gpu},hwdownload,format=nv12"));
        } else {
            chain.push(gpu);
        }
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
/// Shared by both branches above because it is genuinely the same step. On a
/// vendor GPU path the frame is downloaded immediately before this filter and
/// uploaded immediately after it; scale + tone-map remain on the GPU.
fn with_subtitles(mut chain: Vec<String>, opts: &TranscodeOptions, source_path: &str) -> String {
    if let Some(burn) = &opts.subtitle_burn {
        // A bitmap burn is not part of this chain: it is a second stream
        // composited over the chain's output, built by `bitmap_overlay`.
        if !burn.bitmap {
            if let Some(path) = &opts.subtitle_file {
                let escaped = escape_filter_path(&path.to_string_lossy());
                chain.push(format!("subtitles='{escaped}'"));
            } else {
                let escaped = escape_filter_path(source_path);
                chain.push(format!(
                    "subtitles='{escaped}':si={idx}",
                    idx = burn.subtitle_index
                ));
            }
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
/// The HDR format a session should be ROUTED by, as opposed to what the file
/// is.
///
/// A Dolby Vision source whose base layer is HDR10-compatible (Profiles 7/8
/// with the cross-compatible id — the probe's rich label says so) decodes to
/// ordinary PQ surfaces carrying HDR10 static metadata: exactly what an hdr10
/// file decodes to, because the BL *is* a compliant HDR10 stream by
/// construction. Neither tone-map chain reads the RPU dynamic metadata — the
/// CPU chain declares `tin=smpte2084` and maps statically, and the vendor
/// graphs do the same from surface metadata — so for graph selection this IS
/// an hdr10 source, and declining the 4.9× GPU tone-map for it (PERF-PLAN §5)
/// bought no correctness. Profiles without that base (5: IPTPQc2) keep
/// routing as `dolby_vision`, which the vendor graphs correctly decline.
///
/// Routing only: everything that *builds* filters keeps the file's real
/// `hdr` ("dolby_vision" still picks `tin=smpte2084`, still strips RPUs on
/// the copy path), and the cache digest keys on the pipeline that actually
/// ran, so entries made under either routing stay distinct and honest.
pub fn routing_hdr(file: &MediaFile) -> Option<&str> {
    match file.hdr.as_deref() {
        Some("dolby_vision")
            if file
                .hdr_format
                .as_deref()
                .is_some_and(|f| f.contains("HDR10-compatible")) =>
        {
            Some("hdr10")
        }
        other => other,
    }
}

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

/// The current playback's A/V-sync correction as an audio filter, for a session whose
/// audio is being ENCODED anyway. Same sign convention as the `-itsoffset`
/// input it replaces: positive = delay audio.
///
/// The two-input technique — the same file opened again, shifted, used only
/// for its audio — exists because a COPIED audio stream cannot take a
/// filter: copy moves packets and filters need frames. But every encoded
/// path was paying its price too: a second demuxer reading the whole source
/// again, which on NAS media doubles the read traffic and competes with the
/// video path for the same spindle (review §3.4). Where the audio is
/// encoded, the correction is a one-line filter on the single input:
/// `adelay` to push audio later, `atrim` + `asetpts` to pull it earlier
/// (the trimmed head is exactly what fell before the video's start under
/// `-itsoffset`, capped by the ±15s the API allows). Copy semantics keep
/// the second input, and nothing else does.
pub fn audio_offset_filter(offset_ms: i64) -> Option<String> {
    match offset_ms.cmp(&0) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(format!("adelay={offset_ms}:all=1")),
        std::cmp::Ordering::Less => Some(format!(
            "atrim=start={:.3},asetpts=PTS-STARTPTS",
            -offset_ms as f64 / 1000.0
        )),
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

    // A per-file A/V sync correction. The audio on this path is ALWAYS
    // re-encoded (AAC below), so the correction is an audio filter on the one
    // input — the second `-itsoffset` input this used to open read the whole
    // source again for nothing but its audio (review §3.4). Guarded on the
    // file actually having audio: a simple `-af` with no audio stream to
    // attach to is a hard ffmpeg error, where the old optional map was inert.
    let audio_offset = if source.audio_streams.is_empty() {
        None
    } else {
        audio_offset_filter(source.audio_offset_ms)
    };

    // Video filter chain: [hwdownload for GPU-decoded frames →] scale / tonemap /
    // subs [→ GPU upload suffix for VAAPI/QSV] → encoder.
    //
    // A vendor GPU pipeline skips both ends. Its output is already a surface
    // of its encoder's own family, so the upload suffix would be uploading
    // something that never came down — which is not a wasted copy but a broken
    // graph. The neutral pipelines download explicitly inside their own chain
    // and then take the suffix like the CPU path does. Any subtitle burn on a
    // vendor pipeline is the exception both ways: `video_filters` appended a
    // download for libass/overlay, so the encoder's upload IS owed again.
    let subtitle_burn = opts.subtitle_burn.is_some();
    let vendor_gpu =
        matches!(opts.pipeline, Pipeline::VppQsv | Pipeline::TonemapVaapi) && !subtitle_burn;
    let suffix = encoder.filter_suffix().filter(|_| !vendor_gpu);
    let mut vf = String::new();
    if let Some(prefix) = &hwdownload {
        vf.push_str(prefix);
        vf.push(',');
    }
    vf.push_str(&video_filters(source, opts, &source_path));

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
        Some(i) => args.push(format!("0:a:{i}?")),
        None => args.push("0:a:0?".to_owned()),
    }

    match overlay {
        Some(sub) => {
            // The upload suffix comes AFTER the composite. `overlay` draws in
            // system memory; a chain that uploads first hands it a hardware
            // surface it cannot read. It used to be appended to the [vburn]
            // half of this very graph — upload, then overlay — which is that
            // broken order exactly, hidden by the tests only ever burning
            // with the suffix-less software encoder.
            let up = suffix.map(|s| format!(",{s}")).unwrap_or_default();
            args.push("-filter_complex".into());
            args.push(format!(
                "[0:v]{vf}[vburn];{sub};\
                 [vburn][sburn]overlay=eof_action=pass{up}{BURNED_VIDEO_LABEL}"
            ));
        }
        None => {
            if let Some(s) = suffix {
                vf.push(',');
                vf.push_str(s);
            }
            args.push("-vf".into());
            args.push(vf);
        }
    }
    args.extend(encoder.encode_args(
        opts.video_bitrate_kbps,
        opts.effective_rate_control,
        opts.force_idr,
        opts.software_threads,
    ));

    // Segment-aligned keyframes so each segment is independently decodable.
    args.push("-force_key_frames".into());
    args.push(format!("expr:gte(t,n_forced*{SEGMENT_SECONDS})"));

    // Audio: downmix + AAC (browser-universal), with the A/V correction as
    // a filter on the same input rather than a second read of the source.
    if let Some(af) = audio_offset {
        args.push("-af".into());
        args.push(af);
    }
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-ac".into());
    args.push(opts.audio_channels.to_string());
    args.push("-b:a".into());
    args.push(format!("{}k", opts.audio_bitrate_kbps));

    // Start the MPEG-TS timeline at zero.
    //
    // ffmpeg's mpegts muxer defaults to `muxpreload 0.5` + `muxdelay 0.7`, so
    // the first video PTS in segment 0 lands at ~1.4 s (measured, this exact
    // argument list, ffmpeg 6.1: 1.480 PTS / 1.400 DTS before, 0.080 / 0.000
    // after). Nothing in the video path cared — the player seeks by playlist
    // time — but the native subtitle slicer anchors each WebVTT segment with
    // `X-TIMESTAMP-MAP=MPEGTS:{segment_start × 90000}`, i.e. it asserts that
    // segment 0 begins at PTS 0. That assertion was false by 1.4 s, and every
    // native cue on a transcode session rendered 1.4 s early. Fixing the map
    // instead would encode a muxer default into the subtitle path; making the
    // assertion true is the smaller, truer change.
    args.push("-muxdelay".into());
    args.push("0".into());
    args.push("-muxpreload".into());
    args.push("0".into());

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
///
/// A copied video can only begin at the preceding keyframe. When audio is
/// encoded, ffmpeg's default accurate input seek discards audio up to the
/// requested timestamp while retaining that video preroll, leaving the two
/// tracks out of sync. Copy sessions therefore use `-noaccurate_seek` so both
/// tracks retain the same preroll and `-avoid_negative_ts make_zero` can shift
/// them onto one shared output timeline.
/// ffprobe arguments that answer the one question a copy session cannot
/// answer from its own request: **where does its timeline actually begin in
/// the source?**
///
/// A copy session seeks with `-noaccurate_seek` (see below), so the media it
/// emits starts at the keyframe *preceding* the requested offset, and
/// `-avoid_negative_ts make_zero` then relabels that keyframe as t=0. The
/// requested start and the real origin therefore differ by anything from zero
/// to a full GOP — 1–6 s on a 4K film — and anything computed against the
/// requested start (subtitle cue shifts, most of all) is wrong by exactly that
/// much.
///
/// `-read_intervals "{start}%+#4"` makes ffprobe perform the same backward
/// seek the demuxer will, then read four packets: no decoding, no scan, one
/// seek and a handful of packets even on a 19 GB remux over a NAS.
pub fn keyframe_probe_args(source_path: &str, start_seconds: f64) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        "v:0".into(),
        "-show_entries".into(),
        "packet=pts_time,dts_time,flags".into(),
        "-read_intervals".into(),
        format!("{start_seconds:.3}%+#4"),
        "-of".into(),
        "csv=p=0".into(),
        source_path.to_owned(),
    ]
}

/// The source-timeline origin implied by [`keyframe_probe_args`] output.
///
/// Prefers the first keyframe packet's decode timestamp, because that is the
/// value `-avoid_negative_ts make_zero` subtracts to build the output
/// timeline; the presentation timestamp is the fallback for a demuxer that
/// reports no DTS. `None` means the probe said nothing usable, and the caller
/// should fall back to the requested start — which is today's behaviour, so a
/// failed probe is no worse than not probing.
pub fn parse_keyframe_origin(stdout: &str) -> Option<f64> {
    let rows = stdout.lines().filter_map(|line| {
        let mut fields = line.trim().split(',');
        let pts = fields.next()?.trim();
        let dts = fields.next()?.trim();
        let flags = fields.next().unwrap_or("").trim();
        let timestamp = dts
            .parse::<f64>()
            .ok()
            .or_else(|| pts.parse::<f64>().ok())?;
        Some((timestamp, flags.contains('K')))
    });
    let mut first = None;
    for (timestamp, keyframe) in rows {
        if keyframe {
            return Some(timestamp.max(0.0));
        }
        first.get_or_insert(timestamp.max(0.0));
    }
    first
}

pub fn copy_input_seek_args(start_seconds: f64) -> Vec<String> {
    if start_seconds <= 0.0 {
        return Vec::new();
    }
    vec![
        "-noaccurate_seek".into(),
        "-ss".into(),
        format!("{start_seconds:.3}"),
    ]
}

fn copy_audio_channels(source: &MediaFile, audio_index: Option<i64>) -> Option<i64> {
    match audio_index {
        Some(index) => source
            .audio_streams
            .iter()
            .find(|stream| stream.index == index),
        None => source.audio_streams.first(),
    }
    .and_then(|stream| stream.channels)
}

fn copy_input_args(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
    preserve_dolby_vision: bool,
) -> Vec<String> {
    let source_path = source.path.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];

    // Fast input seek for resume/session start. The non-accurate mode keeps
    // encoded audio aligned with the keyframe preroll of the copied video.
    args.extend(copy_input_seek_args(start_seconds));
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

    // A per-file A/V sync correction (positive = audio later). COPIED audio
    // cannot take a filter — copy moves packets, filters need frames — so
    // that case keeps the second `-itsoffset`'d input of the same file, used
    // only for its audio. Audio that is being transcoded anyway takes the
    // correction as a filter on the one input instead: the second demuxer
    // read the whole source again — on NAS media, double the read traffic in
    // exactly the sessions that need the disk most (review §3.4).
    let has_offset = source.audio_offset_ms != 0 && !source.audio_streams.is_empty();
    let audio_input = if has_offset && !transcode_audio {
        args.extend(copy_input_seek_args(start_seconds));
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
        args.push(
            hevc_copy_tag_for_format(
                source.hdr.as_deref(),
                source.hdr_format.as_deref(),
                preserve_dolby_vision,
            )
            .into(),
        );
        // FFmpeg's MOV muxer guards dvcC/dvvC behind `unofficial`. Without
        // this, it keeps the Dolby Vision RPUs and writes a `dvh1` sample
        // entry but silently omits the decoder configuration box. A media
        // playlist can then appear to play through the HDR10 base layer,
        // while an HLS master that correctly advertises `dvh1.08.06` fails
        // AVPlayer with CoreMedia -12927. The strictness option is scoped to
        // preserved DV; ordinary HEVC and stripped HDR10 copies do not need
        // an experimental muxer feature.
        if source.hdr.as_deref() == Some("dolby_vision") && preserve_dolby_vision {
            args.push("-strict".into());
            args.push("unofficial".into());
        }
        args.push("-bsf:v".into());
        args.push(hevc_copy_bsf_for_client(
            source.hdr.as_deref(),
            have_dovi_bsf,
            preserve_dolby_vision,
        ));
    }

    if transcode_audio {
        // The correction rides the encode as a filter — same input, no
        // second read.
        if has_offset {
            if let Some(af) = audio_offset_filter(source.audio_offset_ms) {
                args.push("-af".into());
                args.push(af);
            }
        }
        // e.g. DTS/TrueHD → AAC. Six-channel disc audio is commonly tagged
        // `5.1(side)`. ffmpeg's AAC encoder represents that layout with an
        // in-band Program Config Element (channel_configuration=0), while its
        // fragmented-MP4 sample entry still declares two channels. AVPlayer
        // rejects that contradictory initialization record with CoreMedia
        // -12848 before it opens the video. Standardize six-channel output on
        // AAC's channel_configuration=6 layout; the rematrix only moves the
        // surround pair from side to back labels and preserves 5.1 playback.
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        if copy_audio_channels(source, audio_index) == Some(6) {
            // Apple's HLS authoring recommendation for 5.1 AAC is 320 kbit/s.
            args.push("320k".into());
            args.push("-channel_layout:a".into());
            args.push("5.1".into());
        } else {
            args.push("256k".into());
        }
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
///
/// Apple HLS deliberately does not use edit lists for fragmented MP4
/// compatibility. Generic MP4 muxing otherwise writes `edts`/`elst` boxes to
/// express the seek preroll; AVPlayer accepts that file directly but rejects
/// the same rendition during multivariant HLS validation. Keep the normalized
/// fragment decode times and suppress those movie-level edits.
pub fn copy_pipe_args(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
) -> Vec<String> {
    copy_pipe_args_with_dolby_vision(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        have_dovi_bsf,
        false,
    )
}

pub fn copy_pipe_args_with_dolby_vision(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    have_dovi_bsf: bool,
    preserve_dolby_vision: bool,
) -> Vec<String> {
    let mut args = copy_input_args(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        have_dovi_bsf,
        preserve_dolby_vision,
    );
    args.extend(
        [
            "-avoid_negative_ts",
            "make_zero",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+delay_moov",
            "-use_editlist",
            "0",
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
    hls_copy_args_with_dolby_vision(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        DolbyVisionCopyOptions::new(have_dovi_bsf, false),
        out_dir,
    )
}

/// Dolby Vision handling for an HLS copy session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DolbyVisionCopyOptions {
    have_bsf: bool,
    preserve: bool,
}

impl DolbyVisionCopyOptions {
    pub const fn new(have_bsf: bool, preserve: bool) -> Self {
        Self { have_bsf, preserve }
    }
}

pub fn hls_copy_args_with_dolby_vision(
    source: &MediaFile,
    start_seconds: f64,
    audio_index: Option<i64>,
    transcode_audio: bool,
    pacing: Pacing,
    dolby_vision: DolbyVisionCopyOptions,
    out_dir: &str,
) -> Vec<String> {
    let mut args = copy_input_args(
        source,
        start_seconds,
        audio_index,
        transcode_audio,
        pacing,
        dolby_vision.have_bsf,
        dolby_vision.preserve,
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

    /// A bitmap burn no longer costs the GPU tone-map (the 4.9× of PERF-PLAN
    /// §5): the vendor graph runs scale + tone-map on the GPU pinned to the
    /// overlay's exact frame, comes down once for the composite, and the
    /// encoder's upload runs after the overlay — never before it.
    #[test]
    fn a_bitmap_burn_keeps_the_gpu_tonemap_and_downloads_once() {
        let mut f = file(Some("dolby_vision"));
        f.width = Some(3840);
        f.height = Some(2160);
        f.bit_depth = Some(10);
        let opts = TranscodeOptions {
            target_height: 2160,
            pipeline: Pipeline::VppQsv,
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 5,
                bitmap: true,
            }),
            ..Default::default()
        };
        let args = hls_args(&f, Encoder::Qsv, &opts, Pacing::unpaced(), "/tmp/s");
        let joined = args.join(" ");

        // The GPU graph is present, pinned to the overlay's frame, and comes
        // down to system memory exactly once, before the composite.
        assert!(
            joined.contains(
                "vpp_qsv=w=3840:h=2160:tonemap=1:format=nv12,hwdownload,format=nv12[vburn]"
            ),
            "{joined}"
        );
        // The subtitle plane lands on the same geometry.
        assert!(joined.contains("[0:s:5]scale=3840:2160[sburn]"), "{joined}");
        // The encoder's upload happens AFTER the overlay — a chain that
        // uploads first hands `overlay` a hardware surface it cannot read.
        assert!(
            joined.contains("overlay=eof_action=pass,hwupload=extra_hw_frames=64,format=qsv[vout]"),
            "{joined}"
        );
        let up = joined.find("hwupload").expect("upload present");
        let ov = joined.find("overlay=").expect("overlay present");
        assert!(ov < up, "upload must follow the composite: {joined}");
        // No float tone-map anywhere near this session.
        assert!(!joined.contains("zscale"), "{joined}");
        assert!(!joined.contains("tonemap=tonemap=hable"), "{joined}");
    }

    /// The same ordering rule on the CPU chain: a QSV encode of a burned
    /// session uploads after the composite, not before it. (The old graph put
    /// the encoder suffix on the [vburn] half — upload, then overlay — an
    /// order only the suffix-less software encoder could survive, which is
    /// exactly what the tests used to burn with.)
    #[test]
    fn the_upload_suffix_follows_the_composite_on_the_cpu_chain_too() {
        let mut f = file(Some("hdr10"));
        f.width = Some(3840);
        f.height = Some(2160);
        f.bit_depth = Some(10);
        let opts = TranscodeOptions {
            target_height: 1080,
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 0,
                bitmap: true,
            }),
            ..Default::default()
        };
        let args = hls_args(&f, Encoder::Qsv, &opts, Pacing::unpaced(), "/tmp/s");
        let joined = args.join(" ");
        assert!(
            joined.contains("overlay=eof_action=pass,hwupload=extra_hw_frames=64,format=qsv[vout]"),
            "{joined}"
        );
        assert!(
            !joined.contains("format=qsv[vburn]"),
            "the upload crept back in front of the overlay: {joined}"
        );
        // Without a burn the suffix stays at the end of the plain chain.
        let plain = TranscodeOptions {
            target_height: 1080,
            ..Default::default()
        };
        let joined = hls_args(&f, Encoder::Qsv, &plain, Pacing::unpaced(), "/tmp/s").join(" ");
        assert!(joined.contains("-vf "), "{joined}");
        assert!(
            joined.contains("hwupload=extra_hw_frames=64,format=qsv"),
            "{joined}"
        );
    }

    /// A Dolby Vision source with an HDR10-compatible base layer routes like
    /// the HDR10 stream its base layer is; one without keeps the DV routing
    /// the vendor graphs decline. The file's real `hdr` column is untouched —
    /// this is about graph selection, not about what the file is.
    #[test]
    fn dv_with_an_hdr10_base_routes_as_hdr10() {
        let mut f = file(Some("dolby_vision"));
        f.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".into());
        assert_eq!(routing_hdr(&f), Some("hdr10"));
        f.hdr_format = Some("Dolby Vision · Profile 5".into());
        assert_eq!(routing_hdr(&f), Some("dolby_vision"));
        f.hdr_format = None;
        assert_eq!(routing_hdr(&f), Some("dolby_vision"));
        let f = file(Some("hdr10"));
        assert_eq!(routing_hdr(&f), Some("hdr10"));
        let f = file(None);
        assert_eq!(routing_hdr(&f), None);
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
    fn cached_text_burn_keeps_gpu_tonemap_and_round_trips_once() {
        let mut f = file(Some("dolby_vision"));
        f.width = Some(3840);
        f.height = Some(2160);
        f.bit_depth = Some(10);
        let opts = TranscodeOptions {
            target_height: 1080,
            pipeline: Pipeline::VppQsv,
            subtitle_burn: Some(SubtitleBurn {
                subtitle_index: 2,
                bitmap: false,
            }),
            subtitle_file: Some(PathBuf::from("/cache/scary-forced.vtt")),
            ..Default::default()
        };
        let joined = hls_args(&f, Encoder::Qsv, &opts, Pacing::unpaced(), "/tmp/s").join(" ");

        assert!(
            joined.contains(
                "vpp_qsv=w=-1:h=1080:tonemap=1:format=nv12,hwdownload,format=nv12,subtitles='/cache/scary-forced.vtt',hwupload=extra_hw_frames=64,format=qsv"
            ),
            "{joined}"
        );
        assert_eq!(joined.matches("hwdownload").count(), 1, "{joined}");
        assert_eq!(joined.matches("hwupload").count(), 1, "{joined}");
        assert!(!joined.contains("zscale"), "{joined}");
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

    #[test]
    fn six_channel_copy_audio_uses_an_explicit_standard_aac_layout() {
        let mut media = file(Some("dolby_vision"));
        media.audio_streams = vec![
            crate::domain::AudioStream {
                index: 0,
                codec: "ac3".into(),
                channels: Some(2),
                ..Default::default()
            },
            crate::domain::AudioStream {
                index: 1,
                codec: "dts".into(),
                channels: Some(6),
                ..Default::default()
            },
        ];

        let surround = hls_copy_args(
            &media,
            0.0,
            Some(1),
            true,
            Pacing::unpaced(),
            true,
            "/tmp/s",
        )
        .join(" ");
        assert!(surround.contains("-c:a aac -b:a 320k -channel_layout:a 5.1"));

        let stereo = hls_copy_args(
            &media,
            0.0,
            Some(0),
            true,
            Pacing::unpaced(),
            true,
            "/tmp/s",
        )
        .join(" ");
        assert!(stereo.contains("-c:a aac -b:a 256k"));
        assert!(!stereo.contains("-channel_layout:a"));
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

        // A backward-compatible profile the client accepts keeps its hvc1
        // base sample entry and its RPU/EL units. Apple uses the base entry
        // together with supplemental codec signaling to engage Dolby Vision.
        let mut compatible_dv = file(Some("dolby_vision"));
        compatible_dv.hdr_format = Some("Dolby Vision · Profile 8 (HDR10-compatible)".into());
        let preserved = hls_copy_args_with_dolby_vision(
            &compatible_dv,
            0.0,
            None,
            true,
            Pacing::unpaced(),
            DolbyVisionCopyOptions::new(true, true),
            "/tmp/s",
        )
        .join(" ");
        assert!(preserved.contains("-tag:v hvc1"));
        assert!(preserved.contains("-strict unofficial"));
        assert!(preserved.contains("-bsf:v filter_units=remove_types=32-34"));
        assert!(!preserved.contains("62-63"));
        assert!(!preserved.contains("dovi_rpu=strip=1"));
        assert!(!dv7.contains("-strict unofficial"));

        let profile5 = hls_copy_args_with_dolby_vision(
            &file(Some("dolby_vision")),
            0.0,
            None,
            true,
            Pacing::unpaced(),
            DolbyVisionCopyOptions::new(true, true),
            "/tmp/s",
        )
        .join(" ");
        assert!(profile5.contains("-tag:v dvh1"));

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

    /// A copy-video seek may start at the prior keyframe. Accurate input seek
    /// would discard encoded audio until the requested instant but retain that
    /// video preroll, which is an immediate A/V offset after every seek.
    #[test]
    fn copy_seek_preserves_audio_preroll() {
        let args = hls_copy_args(
            &file(None),
            32.125,
            None,
            true,
            Pacing::unpaced(),
            false,
            "/tmp/s",
        );
        let ss = args.iter().position(|a| a == "-ss").expect("has -ss");
        assert_eq!(args[ss - 1], "-noaccurate_seek");
        assert_eq!(args[ss + 1], "32.125");
        let input = args.iter().position(|a| a == "-i").expect("has input");
        assert!(ss < input, "seek flags must remain input options");

        assert!(copy_input_seek_args(0.0).is_empty());
        assert!(copy_input_seek_args(-1.0).is_empty());
    }

    #[test]
    fn copy_pipe_uses_the_edit_free_apple_hls_timeline() {
        let args = copy_pipe_args(&file(None), 30.0, None, false, Pacing::unpaced(), false);
        let option = args
            .iter()
            .position(|arg| arg == "-use_editlist")
            .expect("Apple HLS edit-list policy");
        assert_eq!(args.get(option + 1).map(String::as_str), Some("0"));
    }

    /// Exercise the actual ffmpeg boundary that exposed the bug: the seek is
    /// between video keyframes and audio is re-encoded. The first timestamps
    /// must still land together rather than leaving silence until the exact
    /// requested instant.
    #[test]
    fn copy_seek_emits_aligned_audio_and_video() {
        use crate::testfixtures::{ffmpeg, ffprobe, run, source};
        use std::process::Command;

        let mut media = file(None);
        media.path = source("h264");
        media.video_codec = Some("h264".into());
        let bytes = run(Command::new(ffmpeg()).args(copy_pipe_args(
            &media,
            3.0,
            None,
            true,
            Pacing::unpaced(),
            false,
        )));
        assert!(
            !bytes
                .windows(4)
                .any(|window| window == b"edts" || window == b"elst"),
            "Apple HLS copy output must not carry movie edit lists"
        );
        let output = tempfile::Builder::new()
            .suffix(".mp4")
            .tempfile()
            .expect("temporary remux");
        std::fs::write(output.path(), bytes).expect("write remux");

        let probe = run(Command::new(ffprobe())
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,start_time",
                "-of",
                "json",
            ])
            .arg(output.path()));
        let value: serde_json::Value = serde_json::from_slice(&probe).expect("ffprobe json");
        let starts = value["streams"].as_array().expect("stream list");
        let start = |kind: &str| {
            starts
                .iter()
                .find(|stream| stream["codec_type"] == kind)
                .and_then(|stream| stream["start_time"].as_str())
                .and_then(|time| time.parse::<f64>().ok())
                .unwrap_or_else(|| panic!("{kind} start time in {value}"))
        };
        let skew = (start("video") - start("audio")).abs();
        assert!(skew < 0.150, "A/V starts differ by {skew:.3}s: {value}");
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
        f.audio_offset_ms = 250;
        // Copied audio: the one case that still forces the second input —
        // and an offset only applies to a file that has audio at all.
        f.audio_streams = vec![crate::domain::AudioStream {
            index: 0,
            codec: "dts".into(),
            ..Default::default()
        }];
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

    /// §3.4: where the audio is being encoded anyway, the A/V correction is
    /// a filter on the ONE input — the second `-itsoffset` input read the
    /// whole source again for nothing but its audio. Copy semantics (and
    /// only copy semantics) keep the second input.
    #[test]
    fn an_encoded_audio_offset_needs_no_second_read() {
        let audio = |f: &mut MediaFile| {
            f.audio_streams = vec![crate::domain::AudioStream {
                index: 0,
                codec: "dts".into(),
                ..Default::default()
            }];
        };
        let inputs = |args: &[String]| args.iter().filter(|a| *a == "-i").count();

        // Transcode path: audio is always AAC, so always one input.
        let mut f = file(None);
        f.audio_offset_ms = 250;
        audio(&mut f);
        let args = hls_args(
            &f,
            Encoder::Software,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        assert_eq!(inputs(&args), 1, "no second read of the source");
        assert!(!args.contains(&"-itsoffset".to_owned()));
        let af = args.iter().position(|a| a == "-af").expect("-af");
        assert_eq!(args[af + 1], "adelay=250:all=1", "positive = audio later");
        assert!(
            args.contains(&"0:a:0?".to_owned()),
            "audio maps from input 0"
        );

        // Negative offset: pull audio earlier by trimming its head — what
        // -itsoffset dropped before the video's start anyway.
        f.audio_offset_ms = -500;
        let args = hls_args(
            &f,
            Encoder::Software,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        let af = args.iter().position(|a| a == "-af").expect("-af");
        assert_eq!(args[af + 1], "atrim=start=0.500,asetpts=PTS-STARTPTS");

        // Copy path, audio transcoded (DTS→AAC): same one-input treatment.
        let args = hls_copy_args(&f, 0.0, None, true, Pacing::unpaced(), false, "/tmp/s");
        assert_eq!(
            inputs(&args),
            1,
            "an encoded copy-session audio needs no second read"
        );
        assert!(args.iter().any(|a| a.starts_with("atrim=")));

        // Copy path, audio COPIED: packets cannot be filtered — the second
        // input is retained, exactly as before.
        let args = hls_copy_args(&f, 0.0, None, false, Pacing::unpaced(), false, "/tmp/s");
        assert_eq!(inputs(&args), 2, "copy semantics keep the -itsoffset input");
        assert!(args.contains(&"-itsoffset".to_owned()));
        assert!(!args.contains(&"-af".to_owned()));

        // A file with no audio track: an offset row is metadata about
        // nothing — no filter (a hard error to attach), no second input.
        let mut mute = file(None);
        mute.audio_offset_ms = 250;
        let args = hls_args(
            &mute,
            Encoder::Software,
            &TranscodeOptions::default(),
            Pacing::unpaced(),
            "/tmp/s",
        );
        assert_eq!(inputs(&args), 1);
        assert!(!args.contains(&"-af".to_owned()));
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

    // ---- The tomb for P0-2 and P0-3 ------------------------------------
    //
    // Both of those defects lived in the gap between what the argument-shape
    // tests assert and what the muxers actually emit: the arguments were
    // exactly as intended, and the bytes that came out did not start where
    // every consumer of them assumed. Nothing short of producing a session
    // and measuring it can close that gap, so these tests run ffmpeg.

    fn ffmpeg() -> String {
        std::env::var("PLURX_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
    }

    fn ffprobe() -> String {
        std::env::var("PLURX_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
    }

    /// A real clip with a deliberately LONG GOP, because a short-GOP fixture
    /// hides P0-2: when every second is a keyframe, the requested start and
    /// the keyframe before it are never far enough apart to notice.
    fn write_long_gop_clip(path: &std::path::Path, seconds: u32, gop_seconds: u32) {
        let fps = 25;
        let status = std::process::Command::new(ffmpeg())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=192x108:rate={fps}:duration={seconds}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                &(fps * gop_seconds).to_string(),
                "-keyint_min",
                &(fps * gop_seconds).to_string(),
                "-sc_threshold",
                "0",
                "-c:a",
                "aac",
                "-shortest",
                "-y",
            ])
            .arg(path)
            .status();
        assert!(
            status.map(|s| s.success()).unwrap_or(false),
            "fixture encode failed — this test needs a working ffmpeg"
        );
    }

    /// First video packet timestamps of a produced artefact, `(pts, dts)`.
    fn first_packet_times(path: &std::path::Path) -> (f64, f64) {
        let out = std::process::Command::new(ffprobe())
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time,dts_time",
                "-read_intervals",
                "%+#1",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe the produced segment");
        let text = String::from_utf8_lossy(&out.stdout);
        let row = text.lines().next().unwrap_or_default();
        let mut fields = row.split(',');
        let pts = fields
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .unwrap_or(f64::NAN);
        let dts = fields
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .unwrap_or(f64::NAN);
        (pts, dts)
    }

    fn run_ffmpeg(args: &[String]) {
        let out = std::process::Command::new(ffmpeg())
            .args(args)
            .output()
            .expect("run the produced argument list");
        assert!(
            out.status.success(),
            "ffmpeg refused the produced arguments: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// P0-3's tomb. The subtitle slicer anchors segment 0 with
    /// `X-TIMESTAMP-MAP=MPEGTS:0`, so segment 0 has to actually begin at PTS
    /// 0. ffmpeg's mpegts defaults (`muxpreload` 0.5 + `muxdelay` 0.7) put it
    /// at ~1.4 s instead, and every native cue on a transcode session
    /// rendered that much early. Measured, not asserted from the flag list:
    /// the flag list was never the thing that was wrong.
    #[test]
    fn a_transcode_session_starts_its_mpegts_timeline_at_zero() {
        let dir = tempfile::tempdir().expect("work dir");
        let src = dir.path().join("clip.mp4");
        write_long_gop_clip(&src, 20, 5);

        let mut source = file(None);
        source.path = src.clone();
        source.width = Some(192);
        source.height = Some(108);
        source.hdr = None;
        let out_dir = dir.path().join("tx");
        std::fs::create_dir_all(&out_dir).expect("out dir");
        let opts = TranscodeOptions {
            target_height: 108,
            start_seconds: 12.3,
            tone_map: ToneMap::None,
            ..Default::default()
        };
        run_ffmpeg(&hls_args(
            &source,
            Encoder::Software,
            &opts,
            Pacing::unpaced(),
            &out_dir.to_string_lossy(),
        ));

        let (pts, dts) = first_packet_times(&out_dir.join("seg00000.ts"));
        // One frame of slack at 25 fps, and not a millisecond more: the whole
        // point is that `MPEGTS:0` is a true statement about these bytes.
        assert!(
            dts.abs() <= 0.04 && pts.abs() <= 0.08,
            "segment 0 begins at pts {pts} / dts {dts}, so X-TIMESTAMP-MAP=MPEGTS:0 is a lie \
             and every native cue renders that much early"
        );
    }

    /// P0-2's tomb. A copy session seeks with `-noaccurate_seek`, so the
    /// media it emits begins at the keyframe BEFORE the requested start, and
    /// a cue shift computed from the request leads the picture by the
    /// difference. The probe has to find that keyframe, and the produced
    /// stream has to agree with it.
    #[test]
    fn a_copy_session_begins_at_the_keyframe_before_the_requested_start() {
        let dir = tempfile::tempdir().expect("work dir");
        let src = dir.path().join("clip.mp4");
        // 5 s GOPs, so a start of 12.3 s is 2.3 s past its keyframe.
        write_long_gop_clip(&src, 30, 5);
        let requested = 12.3;

        // What the probe says.
        let probe = std::process::Command::new(ffprobe())
            .args(keyframe_probe_args(&src.to_string_lossy(), requested))
            .output()
            .expect("probe the source");
        assert!(
            probe.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let origin = parse_keyframe_origin(&String::from_utf8_lossy(&probe.stdout))
            .expect("the probe found a keyframe");
        assert!(
            origin < requested - 1.0,
            "a 5 s-GOP fixture must put the keyframe well before a 12.3 s start, got {origin}"
        );

        // What the session actually produces. The honest measure is how much
        // media comes out: a copy that begins at the keyframe emits
        // `duration - origin` seconds, not `duration - requested`.
        let mut source = file(None);
        source.path = src.clone();
        source.width = Some(192);
        source.height = Some(108);
        source.video_codec = Some("h264".into());
        source.hdr = None;
        source.audio_streams = vec![crate::domain::AudioStream {
            index: 0,
            codec: "aac".into(),
            channels: Some(1),
            default: true,
            ..Default::default()
        }];
        let out_dir = dir.path().join("cp");
        std::fs::create_dir_all(&out_dir).expect("out dir");
        run_ffmpeg(&hls_copy_args(
            &source,
            requested,
            None,
            false,
            Pacing::unpaced(),
            false,
            &out_dir.to_string_lossy(),
        ));

        let playlist =
            std::fs::read_to_string(out_dir.join("index.m3u8")).expect("copy session playlist");
        let produced: f64 = playlist
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .filter_map(|value| {
                value
                    .split_once(',')
                    .map_or(value, |(duration, _)| duration)
                    .parse::<f64>()
                    .ok()
            })
            .sum();
        let from_origin = 30.0 - origin;
        assert!(
            (produced - from_origin).abs() < 0.5,
            "the session produced {produced}s of media; beginning at the probed origin \
             ({origin}s) predicts {from_origin}s. If it instead matches {}s, the media \
             begins at the REQUEST and the probe is wrong; if neither, the seek changed.",
            30.0 - requested
        );
        // And the defect restated as the assertion that would have failed
        // before the fix: shifting cues by the request rather than the origin
        // moves every one of them this far off the picture.
        assert!(
            requested - origin > 1.0,
            "this fixture must actually exercise the lead, got {}s",
            requested - origin
        );
    }
}
