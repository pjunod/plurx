//! On-the-fly HLS transcode sessions.
//!
//! When the decision engine says a file must be transcoded (HEVC/4K/HDR the
//! device can't take), we spawn one ffmpeg per session producing HLS segments
//! into a temp dir, and serve the playlist and segments over HTTP. Sessions
//! are reaped when idle. This is the session-based model; the deterministic
//! per-segment model that enables cluster failover is Phase 3's spike.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plurx_core::domain::PlaybackEvent;
use plurx_core::store::{keys, Store};
use plurx_core::transcode::{
    self, EffectiveRateControl, Encoder, EncoderCaps, Pacing, Pipeline, PipelineDigest,
    QualityRateControlValidation, QualityRc, RateMode, Recipe, ToneMap, TranscodeOptions,
};
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::admission::{
    Admission, Admissions, HwSlot, Priority, Workload, DEFAULT_MAX_HW_SESSIONS, QUEUE_WAIT,
};
use crate::copyseg;
use crate::ffmpeg::{ffmpeg_bin, pacing_caps};
use crate::meter::Meter;

/// Idle timeout after which a session's ffmpeg is killed and its dir removed.
const SESSION_IDLE_SECS: u64 = 60;
/// What "Auto" resolves to — see [`TranscodeManager::auto_height`].
const AUTO_SOFTWARE_HEIGHT: i64 = 720;
const AUTO_HARDWARE_MAX_HEIGHT: i64 = 1080;
/// Floor for any requested rung. Below this there is no picture worth the
/// session, and the ladder's lowest step is 480p anyway.
pub const MIN_HEIGHT: i64 = 144;
/// Ceiling for an explicitly requested rung. Auto never reaches it; the
/// quality menu can.
pub const MAX_HEIGHT: i64 = 2160;
/// How long a segment request waits for ffmpeg to produce a not-yet-written
/// segment before giving up.
const SEGMENT_WAIT: Duration = Duration::from_secs(20);
/// Hold the first live transcode playlist until it has both two complete
/// segments and this much published media. The first playlist used to expose
/// one ~2 s segment while ffmpeg was already writing the rest; hls.js reached
/// that edge at the same instant it scheduled its first EVENT reload and
/// visibly stalled even when the encoder had finished the entire title.
///
/// Two nominal segments are the smallest useful head start. Media duration is
/// the actual contract rather than `TARGETDURATION` (a ceiling, not inventory),
/// and requiring two entries prevents one unusually long opening segment from
/// recreating the same live-edge race. Finished short titles escape through
/// `ENDLIST` rather than waiting for media that can never exist.
const TRANSCODE_START_CUSHION_MS: i64 = transcode::SEGMENT_SECONDS as i64 * 2 * 1_000;
/// The initial playlist request was already bounded to 30 seconds. Naming the
/// cadence keeps the new cushion inside that same failure/cancellation surface:
/// no detached task survives a dropped HTTP request and a failed session still
/// exits immediately.
const PLAYLIST_WAIT_POLLS: usize = 300;
const PLAYLIST_WAIT_POLL: Duration = Duration::from_millis(100);
/// Grace period for a hardware transcode to list its first segment before we
/// assume it stalled (GPU contention, or a decode the GPU can't do) and fall
/// back to software. Longer than a healthy hardware start (~1–3 s), with slack
/// for a 4K decode to ramp.
const FIRST_SEGMENT_GRACE: Duration = Duration::from_secs(12);
/// After falling back to software, how long to wait for real output before
/// declaring the session failed. Software-decoding 4K is slow, so this is
/// generous — but a session that can't produce a first segment in this window
/// is unwatchable, and failing it gives the client a clear error instead of an
/// endless gray screen (e.g. a Dolby Vision stream the build can't decode).
const SOFTWARE_GRACE: Duration = Duration::from_secs(30);
/// How long ffmpeg's output timestamp may sit still before the session counts
/// as stuck rather than slow. ffmpeg emits a `-progress` block about twice a
/// second while it is working at all, so a frozen `out_time` for this long is
/// a wedged pipeline — not a 4K decode losing to the clock, which advances
/// steadily however far behind realtime it falls.
const PROGRESS_STALL: Duration = Duration::from_secs(10);
/// How often the stall watchdog re-asks, once past its initial grace.
const WATCHDOG_POLL: Duration = Duration::from_secs(5);
/// How far behind the *download frontier* media is kept on disk, and why that
/// is not the same as "behind the playhead".
///
/// An HLS session's playlist grows for its whole life (event type), so without
/// pruning a full watch accumulates every segment — cheap at 720p, ~17 GB for
/// a 4K copy. The subtlety is what to measure from. The server knows the
/// highest segment a client has *fetched*, and a client fetches its whole
/// forward buffer ahead of what it is showing. A physical iPad running
/// AVPlayer fetched about 120 seconds ahead even with
/// `preferredForwardBufferDuration = 60`; that preference is not a hard cap.
/// Pruning at a fixed distance behind the frontier therefore deletes media the
/// viewer is about to watch — or is watching. Retention covers the observed
/// fetch lead, the back buffer the client keeps for scrubbing, and an
/// allowance for a retry or a playlist reload landing on something older.
const CLIENT_FORWARD_FETCH_SECS: i64 = 120;
const CLIENT_BACK_BUFFER_SECS: i64 = 30;
const RETRY_ALLOWANCE_SECS: i64 = 30;
const RETENTION_SECS: i64 =
    CLIENT_FORWARD_FETCH_SECS + CLIENT_BACK_BUFFER_SECS + RETRY_ALLOWANCE_SECS;
/// Default pace for an HLS session's input, as a multiple of realtime, and how
/// many seconds it may deliver flat-out first. Admin-overridable (see
/// [`keys::HLS_READRATE`] / [`keys::HLS_BURST_SECS`]).
pub(crate) const HLS_READRATE_DEFAULT: f64 = 2.0;
pub(crate) const HLS_BURST_SECS_DEFAULT: f64 = 90.0;
/// How far ahead of the download frontier a session may produce before it is
/// suspended — in seconds of published media, and in bytes on disk.
///
/// Both, because neither alone is a bound. 180 seconds is a few hundred
/// megabytes at a transcode rung and over a gigabyte of 4K copy, so a
/// time-only limit is not a disk contract; and a byte-only limit would starve
/// a low-bitrate stream of the reserve it could afford.
pub(crate) const HLS_AHEAD_MAX_SECS_DEFAULT: i64 = 180;
pub(crate) const HLS_AHEAD_MAX_BYTES_DEFAULT: i64 = 2 * 1024 * 1024 * 1024;
/// Ceiling on scratch across *all* live sessions. A per-session cap bounds one
/// runaway; it does nothing about four healthy 4K sessions filling the disk
/// between them.
pub(crate) const HLS_SCRATCH_MAX_BYTES_DEFAULT: i64 = 8 * 1024 * 1024 * 1024;
/// How long a snapshot of the ahead-window limits may serve flow control
/// before the settings are consulted again. The bound on how stale an
/// admin's change can look, and the whole cost of caching it.
const AHEAD_LIMITS_TTL: Duration = Duration::from_secs(2);
/// Repair cadence when no client request triggers flow control first. At the
/// default 2× pace this permits at most 30 seconds of additional production
/// between observations; keep the arithmetic pinned below.
const FLOW_CONTROL_REPAIR_INTERVAL: Duration = Duration::from_secs(15);

/// Live encode telemetry for one session, fed by ffmpeg's `-progress` stream.
///
/// Without it, "slow" and "stalled" are the same observation. The only signal
/// the session machinery had was whether a finished segment was listed yet —
/// a yes/no answer to a question that needs a rate. A 4K HDR session
/// tone-mapping at 0.7x and a session whose GPU has wedged look identical for
/// the first twelve seconds, and the watchdog killed both, restarting the
/// merely-slow one on software that is slower still.
#[derive(Debug)]
pub struct Progress {
    /// Monotonic zero point for `moved_at_ms`.
    started: Instant,
    /// Which spawn attempt owns these numbers.
    ///
    /// A killed attempt's stdout reader does not stop the instant the process
    /// does — it can still be draining buffered lines while the replacement is
    /// already running. Without a generation, one of those late lines lands on
    /// the new attempt's telemetry, and a stale `out_time` from a process that
    /// got further along reads as "produced, then frozen": a healthy new
    /// encoder declared stalled by its dead predecessor.
    generation: AtomicU64,
    /// Content produced so far, in ms of output timeline; `-1` before the
    /// first block. Relative to the session's own `-ss`, because an input seek
    /// restarts output timestamps at zero.
    ///
    /// This is ENCODER progress, not published media: it includes frames muxed
    /// into the in-progress `.tmp` segment, which no client can fetch. The
    /// watchdog wants exactly that (proof of motion); pacing must not use it
    /// (that is what the segment index is for).
    out_time_ms: AtomicI64,
    /// Cumulative encode rate x1000 (`speed=1.85x` -> 1850); `-1` when unknown.
    speed_milli: AtomicI64,
    /// `started.elapsed()` when `out_time_ms` last *moved*. Staleness is
    /// measured from here rather than from the last block received: a stuck
    /// ffmpeg keeps emitting blocks, it just stops advancing.
    moved_at_ms: AtomicI64,
    /// Baseline for the recent-rate delta: wall clock and output time at the
    /// last usable sample, or [`SAMPLE_UNSET`] before there is one.
    sample_wall_ms: AtomicI64,
    sample_out_ms: AtomicI64,
    /// Smoothed recent rate x1000; `-1` until two usable samples exist.
    recent_milli: AtomicI64,
}

/// "No baseline yet". A distinct sentinel rather than `-1`, because these two
/// fields are *subtracted* — a sentinel inside the arithmetic's own range is a
/// value that can be silently differenced against a real one.
const SAMPLE_UNSET: i64 = i64::MIN;

/// A sample separated by more than this much wall clock spans a suspend or a
/// stall; dividing content by that gap reports a slowdown that never happened,
/// so such a sample re-baselines instead of contributing.
const RECENT_SAMPLE_MAX_GAP_MS: i64 = 5_000;
/// Samples closer together than this are noise -- ffmpeg emits progress about
/// twice a second and adjacent blocks carry real jitter.
const RECENT_SAMPLE_MIN_MS: i64 = 400;

/// One exponential-moving-average step over a progress sample, or `None` when
/// the sample should not move the average at all.
///
/// Pure, because the two rejection rules are the whole subtlety and they are
/// invisible in a test that has to sleep to produce a sample: a gap longer
/// than the cutoff spans a suspend (dividing its content by the stopped time
/// invents a slowdown the moment a held session resumes), and a gap shorter
/// than the floor is two adjacent ffmpeg blocks whose jitter is larger than
/// the signal.
fn recent_rate_step(prev_ewma: i64, d_wall_ms: i64, d_out_ms: i64) -> Option<i64> {
    if !(RECENT_SAMPLE_MIN_MS..=RECENT_SAMPLE_MAX_GAP_MS).contains(&d_wall_ms) || d_out_ms < 0 {
        return None;
    }
    let instant = (d_out_ms * 1000) / d_wall_ms;
    Some(if prev_ewma < 0 {
        instant
    } else {
        // Weighted toward history: a single slow segment on a variable-bitrate
        // film should bend the number, not spike it.
        (prev_ewma * 7 + instant * 3) / 10
    })
}

impl Progress {
    pub fn new() -> Progress {
        Progress {
            started: Instant::now(),
            generation: AtomicU64::new(0),
            out_time_ms: AtomicI64::new(-1),
            speed_milli: AtomicI64::new(-1),
            moved_at_ms: AtomicI64::new(0),
            sample_wall_ms: AtomicI64::new(SAMPLE_UNSET),
            sample_out_ms: AtomicI64::new(SAMPLE_UNSET),
            recent_milli: AtomicI64::new(-1),
        }
    }

    /// Start a new attempt: forget everything measured so far and return the
    /// generation the new process's reader must quote to be believed.
    ///
    /// Called when a session respawns (the hardware->software fallback). The
    /// replacement writes its own timeline from the same seek point, so
    /// carrying the dead process's numbers would make it look instantly
    /// stalled -- and the generation bump is what stops the dead process's
    /// reader from putting them back.
    pub fn begin_attempt(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Relaxed) + 1;
        self.out_time_ms.store(-1, Relaxed);
        self.speed_milli.store(-1, Relaxed);
        self.recent_milli.store(-1, Relaxed);
        self.sample_wall_ms.store(SAMPLE_UNSET, Relaxed);
        self.sample_out_ms.store(SAMPLE_UNSET, Relaxed);
        self.moved_at_ms
            .store(self.started.elapsed().as_millis() as i64, Relaxed);
        generation
    }

    fn generation(&self) -> u64 {
        self.generation.load(Relaxed)
    }

    fn note_out_time(&self, ms: i64) {
        let now = self.started.elapsed().as_millis() as i64;
        if self.out_time_ms.swap(ms, Relaxed) == ms {
            return; // a repeated timestamp is not progress
        }
        self.moved_at_ms.store(now, Relaxed);

        // Recent rate: content produced per second of wall clock, smoothed.
        // ffmpeg's own `speed=` is cumulative over the whole session, which
        // hides a slowdown behind a fast start and reads as nonsense across a
        // suspend -- and it is the recent number that predicts whether the
        // viewer's reserve is about to drain.
        let last_wall = self.sample_wall_ms.swap(now, Relaxed);
        let last_out = self.sample_out_ms.swap(ms, Relaxed);
        if last_wall == SAMPLE_UNSET || last_out == SAMPLE_UNSET {
            return; // the first sample only establishes a baseline
        }
        let prev = self.recent_milli.load(Relaxed);
        if let Some(next) = recent_rate_step(prev, now - last_wall, ms - last_out) {
            self.recent_milli.store(next, Relaxed);
        }
    }

    /// How long output has not advanced. Also the answer before the first
    /// block ever arrives, which is what makes a session that never opened its
    /// input measurable by the same rule as one that died halfway.
    fn stalled_for(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as i64;
        Duration::from_millis((now - self.moved_at_ms.load(Relaxed)).max(0) as u64)
    }

    pub fn out_time_ms(&self) -> Option<i64> {
        Some(self.out_time_ms.load(Relaxed)).filter(|v| *v >= 0)
    }

    pub fn speed(&self) -> Option<f64> {
        Some(self.speed_milli.load(Relaxed))
            .filter(|v| *v >= 0)
            .map(|v| v as f64 / 1000.0)
    }

    /// The rate over the last few seconds, which is the one that predicts a
    /// stall. Falls back to nothing rather than to the cumulative figure --
    /// reporting a session's lifetime average as "recent" is how a slowdown
    /// stays invisible.
    pub fn recent_speed(&self) -> Option<f64> {
        Some(self.recent_milli.load(Relaxed))
            .filter(|v| *v >= 0)
            .map(|v| v as f64 / 1000.0)
    }

    /// Restart the motion clock without touching anything measured.
    ///
    /// Called when a suspended session is resumed. `moved_at_ms` stopped
    /// advancing the moment the SIGSTOP landed — correctly, nothing was
    /// moving — so the first stall check after SIGCONT would otherwise read
    /// the whole suspension as "output has not advanced for minutes" and fail
    /// a healthy session that simply had not emitted its first post-resume
    /// progress block yet. The recent-rate sampler needs no equivalent: a
    /// sample spanning the suspension is already rejected by
    /// [`RECENT_SAMPLE_MAX_GAP_MS`].
    fn touch(&self) {
        self.moved_at_ms
            .store(self.started.elapsed().as_millis() as i64, Relaxed);
    }
}

/// One completed, published segment — the unit of everything except the
/// watchdog.
#[derive(Debug, Clone, PartialEq)]
struct SegmentMeta {
    index: i64,
    /// The URI exactly as the playlist wrote it, so the file can be found
    /// without guessing an extension (`.ts` for transcode, `.m4s` for copy).
    name: String,
    /// Session-relative bounds, accumulated from `EXTINF`. Never
    /// `index × SEGMENT_SECONDS`: the copy path cannot force keyframes, so its
    /// segments run to the source's GOP and a 2-second target routinely
    /// produces 5- and 10-second segments. Multiplying an index by the target
    /// is a lie on exactly the sessions this accounting exists to bound.
    start_ms: i64,
    end_ms: i64,
    /// Size on disk, or 0 until it has been measured (or after it is pruned).
    bytes: i64,
    /// Retention deleted the file. Without the flag, every refresh re-stats
    /// every pruned segment forever — by the back half of a film that is
    /// hundreds of ENOENTs per refresh, all to relearn `bytes: 0`.
    pruned: bool,
}

/// What a session has actually published, in media time and bytes.
///
/// This is the second of the three frontiers a session has, and the one that
/// pacing uses. The others are ffmpeg's `out_time` (encoder progress —
/// includes the in-progress `.tmp` segment nobody can fetch) and the client's
/// download frontier. Conflating any two of them produces a plausible number
/// that is wrong in a different way each time.
#[derive(Debug, Default)]
struct SegmentIndex {
    segs: Vec<SegmentMeta>,
}

impl SegmentIndex {
    /// End of the newest completed segment: the pacing clock.
    fn produced_playable_end_ms(&self) -> Option<i64> {
        self.segs.last().map(|s| s.end_ms)
    }

    /// Where a given segment ends, for turning "the client fetched segment N"
    /// into a position on the media timeline.
    fn end_ms_of(&self, index: i64) -> Option<i64> {
        self.segs
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.end_ms)
    }

    /// The complete session-relative window for one segment. Pruned entries
    /// stay in the index precisely so internal timelines (notably native
    /// subtitles) do not forget how much media preceded the served window.
    fn window_ms_of(&self, index: i64) -> Option<(i64, i64)> {
        self.segs
            .iter()
            .find(|s| s.index == index)
            .map(|s| (s.start_ms, s.end_ms))
    }

    /// First segment whose bytes are still available to a playlist client.
    fn first_retained_index(&self) -> Option<i64> {
        self.segs.iter().find(|s| !s.pruned).map(|s| s.index)
    }

    /// Bytes of published segments lying entirely after `ms`.
    fn bytes_after_ms(&self, ms: i64) -> i64 {
        self.segs
            .iter()
            .filter(|s| s.start_ms >= ms)
            .map(|s| s.bytes)
            .sum()
    }

    /// Every byte still on disk, wherever the frontier is. Pruned segments
    /// carry zero, so this is what the session actually occupies.
    fn total_bytes(&self) -> i64 {
        self.segs.iter().map(|s| s.bytes).sum()
    }

    /// Segments old enough to delete: those that END before the retention
    /// window opens. A segment straddling the boundary is kept — half a
    /// segment is no use to anyone and the arithmetic is cheap.
    fn prunable(&self, keep_from_ms: i64) -> impl Iterator<Item = &SegmentMeta> {
        self.segs
            .iter()
            .filter(move |s| s.bytes > 0 && s.end_ms <= keep_from_ms)
    }

    /// Bring the index up to date with the playlist text by *appending* what
    /// is newly published, and only that. Returns true when it had to rebuild
    /// instead.
    ///
    /// This replaced re-parsing the complete growing EVENT playlist on every
    /// refresh — reconstructing every prior entry, round-tripping every known
    /// size through a map — which approached quadratic work over a long
    /// session on exactly the hot path that runs on every segment publish
    /// (review §2.6). An EVENT playlist may not mutate published entries, so
    /// known ordinals are counted and skipped without so much as a float
    /// parse; sizes and prune flags stay where they are.
    ///
    /// Two things do force a rebuild, both real: the playlist shrank
    /// (truncation, recovery), or its content disagrees with what is held —
    /// the fallback respawn clears the directory and rewrites the timeline
    /// from the same seek point, reusing the same names, so the sentinel is
    /// the last *known* entry's duration and index rather than the count.
    /// A rebuild drops carried sizes on purpose: they described files a
    /// replaced timeline no longer contains.
    fn extend_from_playlist(&mut self, text: &str) -> bool {
        let known = self.segs.len();
        let mut seen = 0usize;
        let mut pending: Option<&str> = None;
        let mut cursor_ms = self.segs.last().map(|s| s.end_ms).unwrap_or(0);
        let mut disagreed = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("#EXTINF:") {
                pending = Some(rest);
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let (Some(ext), Some(index)) = (pending.take(), segment_index(line)) else {
                continue;
            };
            seen += 1;
            if seen <= known {
                let held = &self.segs[seen - 1];
                // The cheap sentinel on every known ordinal is the index; the
                // one duration parse per refresh is spent on the last known
                // entry, where a rewritten timeline's different cut point
                // shows first.
                if held.index != index
                    || (seen == known
                        && extinf_ms(ext).is_some_and(|d| d != held.end_ms - held.start_ms))
                {
                    disagreed = true;
                    break;
                }
                continue;
            }
            let Some(duration_ms) = extinf_ms(ext) else {
                continue;
            };
            self.segs.push(SegmentMeta {
                index,
                name: line.to_owned(),
                start_ms: cursor_ms,
                end_ms: cursor_ms + duration_ms,
                bytes: 0,
                pruned: false,
            });
            cursor_ms += duration_ms;
        }
        if disagreed || seen < known {
            self.segs = parse_playlist(text);
            return true;
        }
        false
    }
}

/// Parse `EXTINF` durations and segment URIs out of an HLS media playlist.
///
/// Pure, and the reason the copy path's variable segment lengths stop being a
/// guess: the playlist is the only place the true duration of a copied segment
/// is written down.
fn extinf_ms(rest: &str) -> Option<i64> {
    rest.split(',')
        .next()
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| *d >= 0.0)
        .map(|secs| (secs * 1000.0).round() as i64)
}

fn parse_playlist(text: &str) -> Vec<SegmentMeta> {
    let mut out: Vec<SegmentMeta> = Vec::new();
    let mut pending_ms: Option<i64> = None;
    let mut cursor_ms = 0i64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_ms = extinf_ms(rest);
            continue;
        }
        // Every other tag, including `#EXT-X-MAP:URI="init.mp4"`, which names
        // a file that is not a segment and carries no duration.
        if line.starts_with('#') {
            continue;
        }
        // A URI line counts only when an EXTINF introduced it.
        let (Some(duration_ms), Some(index)) = (pending_ms.take(), segment_index(line)) else {
            continue;
        };
        out.push(SegmentMeta {
            index,
            name: line.to_owned(),
            start_ms: cursor_ms,
            end_ms: cursor_ms + duration_ms,
            bytes: 0,
            pruned: false,
        });
        cursor_ms += duration_ms;
    }
    out
}

/// Whether the first HTTP response for a live transcode can safely expose this
/// EVENT playlist. Later reloads never pass through this verdict: the session
/// remembers that publication opened.
fn transcode_first_playlist_ready(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        // Preserve the old fail-fast behavior for malformed ffmpeg output. A
        // media player can report the parse error; hiding it behind a 30-second
        // wait would turn a useful failure into a gray screen.
        return true;
    };
    if text.lines().any(|line| line.trim() == "#EXT-X-ENDLIST") {
        return true;
    }
    let segments = parse_playlist(text);
    segments.len() >= 2
        && segments
            .last()
            .is_some_and(|segment| segment.end_ms >= TRANSCODE_START_CUSHION_MS)
}

/// Turn the append-only writer playlist into the sliding view clients see.
///
/// FFmpeg and the copy segmenter keep an EVENT playlist on disk because the
/// segment index needs the complete duration history. Retention eventually
/// unlinks a prefix of those segment files. Serving that raw EVENT playlist
/// after the unlink advertises media that no longer exists; AVPlayer may ask
/// for one of those stale URIs during a playlist reload or decoder reset and
/// stall even though the current encoder is healthy.
///
/// The settings-gated experiment serves a typeless sliding shape from the
/// first response, with an explicit zero start offset, so the same URL never
/// mutates from EVENT to live semantics under AVPlayer. Otherwise the legacy
/// shape changes only once pruning makes EVENT impossible to serve honestly.
/// The raw file and in-memory index stay complete in both cases.
fn served_live_playlist(
    raw: Vec<u8>,
    first_retained: Option<i64>,
    typeless_sliding: bool,
) -> Vec<u8> {
    let first_retained = first_retained.filter(|index| *index > 0).unwrap_or(0);
    if first_retained == 0 && !typeless_sliding {
        return raw;
    }
    let Ok(text) = std::str::from_utf8(&raw) else {
        return raw;
    };
    let lines: Vec<&str> = text.lines().collect();
    let Some(header_end) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("#EXTINF:"))
    else {
        return raw;
    };

    let body_start = if first_retained == 0 {
        header_end
    } else {
        // Start immediately after the prior segment URI. This retains any tags
        // attached to the first surviving segment rather than assuming EXTINF
        // is always the first line in its block.
        let mut next_block = header_end;
        let mut body_start = None;
        for (position, line) in lines.iter().enumerate().skip(header_end) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if segment_index(line) == Some(first_retained) {
                body_start = Some(next_block);
                break;
            }
            if segment_index(line).is_some() {
                next_block = position + 1;
            }
        }
        let Some(body_start) = body_start else {
            // The index was derived from this playlist, so disagreement means
            // a concurrent truncate/restart. Let the next reload observe the
            // rebuilt index instead of manufacturing mismatched state.
            return raw;
        };
        body_start
    };

    let mut out = String::with_capacity(text.len());
    let mut wrote_media_sequence = false;
    let mut wrote_start = false;
    for line in &lines[..header_end] {
        let trimmed = line.trim();
        if trimmed == "#EXT-X-PLAYLIST-TYPE:EVENT" {
            continue;
        }
        if trimmed.starts_with("#EXT-X-START:") {
            wrote_start = true;
        }
        if trimmed.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_retained}\n"));
            wrote_media_sequence = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !wrote_media_sequence {
        out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_retained}\n"));
    }
    if typeless_sliding && !wrote_start {
        out.push_str("#EXT-X-START:TIME-OFFSET=0\n");
    }
    for line in &lines[body_start..] {
        out.push_str(line);
        out.push('\n');
    }
    out.into_bytes()
}

/// How far a session has run ahead of the client, both ways it can matter.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Ahead {
    seconds: i64,
    bytes: i64,
}

/// The bounds a session's ahead-window is held to.
#[derive(Debug, Clone, Copy)]
struct AheadLimits {
    max_secs: i64,
    max_bytes: i64,
    global_max_bytes: i64,
}

/// The active bound keeping an ahead-window session held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AheadHoldReason {
    Time,
    Bytes,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AheadHold {
    reason: AheadHoldReason,
    release_value: i64,
}

/// Whether a session should be held, given how far ahead it is, how much
/// scratch every session is using between them, and whether it is already
/// held.
///
/// Any single limit is enough to suspend; resuming needs all of them below
/// their release thresholds. Byte budgets release at half because they are
/// hard disk bounds. Media time keeps a 30-second gap from its ceiling (never
/// below half) so a fast encoder does not SIGSTOP/SIGCONT for every client
/// segment near the boundary. A one-second ceiling still releases at one
/// second rather than accidentally disabling itself at zero.
fn time_release_threshold(limit: i64) -> Option<i64> {
    (limit > 0).then(|| limit.saturating_sub(30).max(limit / 2).max(1))
}

fn ahead_hold(
    ahead: Ahead,
    global_live_bytes: i64,
    global_ahead_bytes: i64,
    limits: AheadLimits,
    currently_suspended: bool,
) -> Option<AheadHold> {
    let half = |limit: i64| limit / 2;
    let time_limit = if currently_suspended {
        time_release_threshold(limits.max_secs).unwrap_or(limits.max_secs)
    } else {
        limits.max_secs
    };
    let byte_limit = |limit: i64| {
        if currently_suspended {
            half(limit)
        } else {
            limit
        }
    };
    let over = |value: i64, limit: i64| limit > 0 && value > limit;

    // Capacity holds take precedence in telemetry because they cannot be
    // cleared merely by crossing the media-time release point.
    let global_limit = byte_limit(limits.global_max_bytes);
    // Enter on total scratch so the configured cap remains a real disk
    // bound. Release on the drainable reserve: retained bytes behind every
    // client's frontier cannot be pruned inside RETENTION_SECS, so using
    // total scratch for the half-cap release line can make that line
    // structurally unreachable even after every client has caught up.
    let global_value = if currently_suspended {
        global_ahead_bytes
    } else {
        global_live_bytes
    };
    if over(global_value, global_limit) {
        return Some(AheadHold {
            reason: AheadHoldReason::Global,
            release_value: global_limit,
        });
    }
    let session_byte_limit = byte_limit(limits.max_bytes);
    if over(ahead.bytes, session_byte_limit) {
        return Some(AheadHold {
            reason: AheadHoldReason::Bytes,
            release_value: session_byte_limit,
        });
    }
    over(ahead.seconds, time_limit).then_some(AheadHold {
        reason: AheadHoldReason::Time,
        release_value: time_limit,
    })
}

#[cfg(test)]
fn should_suspend(
    ahead: Ahead,
    global_live_bytes: i64,
    global_ahead_bytes: i64,
    limits: AheadLimits,
    currently_suspended: bool,
) -> bool {
    ahead_hold(
        ahead,
        global_live_bytes,
        global_ahead_bytes,
        limits,
        currently_suspended,
    )
    .is_some()
}

/// Apply one `key=value` line of ffmpeg's `-progress` output.
///
/// Values can be the literal `N/A` (before the first frame lands, and for
/// `speed` on a copy that has not been running long enough to estimate),
/// which parses to nothing rather than to zero — zero would read as "stalled"
/// and zero would read as "not moving" respectively, both wrong.
pub fn apply_progress_line(progress: &Progress, generation: u64, line: &str) {
    // A line from a superseded attempt is not evidence about the running one.
    if progress.generation() != generation {
        return;
    }
    let Some((key, value)) = line.split_once('=') else {
        return;
    };
    let value = value.trim();
    if value.is_empty() || value == "N/A" {
        return;
    }
    match key.trim() {
        // Both are microseconds despite the `_ms` name — an ffmpeg quirk, not
        // a typo here. `out_time_us` is the newer spelling; accept either so
        // this keeps working across builds.
        "out_time_us" | "out_time_ms" => {
            if let Ok(us) = value.parse::<i64>() {
                progress.note_out_time(us / 1000);
            }
        }
        "speed" => {
            if let Ok(x) = value.trim_end_matches('x').parse::<f64>() {
                progress.speed_milli.store((x * 1000.0) as i64, Relaxed);
            }
        }
        _ => {}
    }
}

/// Spawn an ffmpeg HLS transcode, draining its stderr (at `-loglevel error`)
/// into the logs so a failure is visible instead of a silently dead session,
/// and its stdout — which carries `-progress` telemetry — into `progress`.
fn spawn_ffmpeg(
    args: &[String],
    encoder_label: &'static str,
    session_id: &str,
    progress: Arc<Progress>,
    generation: u64,
    runtime_cache: &std::path::Path,
) -> Result<Child, String> {
    // `-progress pipe:1` is a global option, so it can lead the vector; the
    // HLS muxer writes to files, which leaves stdout free to carry it.
    let mut full: Vec<String> = vec!["-progress".into(), "pipe:1".into()];
    full.extend_from_slice(args);
    let mut command = tokio::process::Command::new(ffmpeg_bin());
    configure_ffmpeg_runtime(&mut command, runtime_cache);
    let mut child = command
        .args(&full)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ffmpeg: {e}"))?;
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                apply_progress_line(&progress, generation, &line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let sid = session_id.to_owned();
        let started = Instant::now();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(session = %sid, encoder = encoder_label, "transcode ffmpeg: {line}");
            }
            // Stderr closing means the process ended. Logging it (with how long
            // it ran) distinguishes "ffmpeg died early" from "ffmpeg is still
            // running but produced nothing".
            tracing::warn!(
                session = %sid, encoder = encoder_label,
                elapsed_s = started.elapsed().as_secs(),
                "transcode ffmpeg process ended"
            );
        });
    }
    Ok(child)
}

/// Spawn a copy-session ffmpeg whose **stdout is the media**, not telemetry.
///
/// The one structural difference from [`spawn_ffmpeg`]: with the fragmented
/// stream on stdout, `-progress` has to go somewhere else, so it goes to
/// stderr alongside the log. Progress blocks are `key=value` on a line of
/// their own and ffmpeg's own messages are not, so the drain sorts them and
/// feeds the telemetry the watchdog and the activity page already read. Losing
/// that would leave a segmenter session with no `speed`, no `out_time`, and a
/// stall watchdog with nothing to watch.
fn spawn_ffmpeg_pipe(
    args: &[String],
    session_id: &str,
    progress: Arc<Progress>,
    generation: u64,
    runtime_cache: &std::path::Path,
) -> Result<(Child, tokio::process::ChildStdout), String> {
    let mut full: Vec<String> = vec!["-progress".into(), "pipe:2".into()];
    full.extend_from_slice(args);
    let mut command = tokio::process::Command::new(ffmpeg_bin());
    configure_ffmpeg_runtime(&mut command, runtime_cache);
    let mut child = command
        .args(&full)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ffmpeg: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg started without a stdout pipe".to_owned())?;
    if let Some(stderr) = child.stderr.take() {
        let sid = session_id.to_owned();
        let started = Instant::now();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if is_progress_line(&line) {
                    apply_progress_line(&progress, generation, &line);
                } else {
                    tracing::warn!(session = %sid, encoder = "copy", "transcode ffmpeg: {line}");
                }
            }
            tracing::warn!(
                session = %sid, encoder = "copy",
                elapsed_s = started.elapsed().as_secs(),
                "transcode ffmpeg process ended"
            );
        });
    }
    Ok((child, stdout))
}

/// Give libraries loaded by ffmpeg a cache owned by plurxd.
///
/// Docker deployments commonly override the image user with a numeric host
/// UID while leaving `HOME` out of the daemon environment. A non-login process
/// does not synthesize it from passwd, so fontconfig has no user cache while
/// libass initializes a text-subtitle burn. A transcode can then spend the
/// entire first-segment watchdog window rebuilding font metadata, leaving the
/// player with no HLS resource.
/// Keep the environment local to ffmpeg rather than changing the daemon's
/// process environment, and use the data directory whose ownership plurxd has
/// already proved by creating its session and cache directories.
fn configure_ffmpeg_runtime(
    command: &mut tokio::process::Command,
    runtime_cache: &std::path::Path,
) {
    command.env("XDG_CACHE_HOME", runtime_cache);
}

/// Is this stderr line one of ffmpeg's `-progress` blocks rather than a log
/// message?
///
/// Matched on the key, from the set `-progress` actually emits, rather than on
/// "contains an `=`" — an error message about `filter_units=remove_types=32-34`
/// contains plenty of those, and swallowing it would hide exactly the failure
/// a copy session is most likely to have.
fn is_progress_line(line: &str) -> bool {
    let Some((key, _)) = line.split_once('=') else {
        return false;
    };
    matches!(
        key,
        "frame"
            | "fps"
            | "bitrate"
            | "total_size"
            | "out_time_us"
            | "out_time_ms"
            | "out_time"
            | "dup_frames"
            | "drop_frames"
            | "speed"
            | "progress"
    ) || (key.starts_with("stream_") && key.ends_with("_q"))
}

/// True once the playlist actually lists a finished segment — i.e. real,
/// playable output. This must NOT be "a `seg*` file exists": ffmpeg's HLS muxer
/// opens the next segment as a `.tmp` before any frame is written, so a
/// name-prefix check counts a stalled session as healthy and blinds the
/// fallback watchdog (the exact bug behind a 4K-DV grey screen that spun ffmpeg
/// for a minute and was never retried). A `.ts` line lands in the playlist only
/// when a segment is complete.
async fn session_producing(dir: &std::path::Path) -> bool {
    match tokio::fs::read(dir.join("index.m3u8")).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).lines().any(|l| {
            let l = l.trim();
            // A listed segment: `.ts` (transcode) or `.m4s` (copy fMP4).
            !l.starts_with('#') && (l.ends_with(".ts") || l.ends_with(".m4s"))
        }),
        Err(_) => false,
    }
}

/// What one watchdog poll should do with what it observed. Pure, because the
/// subtlety is entirely in which states are *not* a stall, and each of those
/// is a healthy session the wrong verdict would kill mid-film: a suspended
/// encoder is motionless on purpose, an exited child is EOF or a kill (both
/// already have owners reporting them), and a session failed elsewhere is
/// already being torn down.
#[derive(Debug, PartialEq)]
enum WatchNext {
    /// Nothing wrong, or nothing judgeable: look again next poll.
    Wait,
    /// The watch is over — the process exited or someone else failed the
    /// session. Not a verdict; both cases already have their own reporters.
    Done,
    /// Output stopped advancing while the session was supposed to be moving.
    Stall,
}

fn watch_next(failed: bool, exited: bool, suspended: bool, stalled_for: Duration) -> WatchNext {
    if failed || exited {
        return WatchNext::Done;
    }
    if !suspended && stalled_for >= PROGRESS_STALL {
        return WatchNext::Stall;
    }
    WatchNext::Wait
}

/// Fail a session whose output has stopped advancing — for the life of the
/// encoder, not just its start.
///
/// This replaced a single verdict taken at a fixed deadline ("no segment after
/// 30s ⇒ dead"), which could not tell a wedged pipeline from a slow one and
/// killed both. It answers a different question — *is output still moving?* —
/// on a poll, which is strictly more informative in both directions: a session
/// that never opened its input still fails at the same deadline, and a session
/// decoding 4K at 0.4x is left alone.
///
/// It used to return at the first playable segment, which left every *later*
/// wedge unwatched: a pipeline that froze at minute 40 drained the client's
/// buffer and then hung the player on a segment nobody was writing (review
/// §2.3). Now the watch ends only when the process exits (EOF and kills both
/// have owners reporting them) or the session is failed elsewhere. The states
/// that are deliberately never judged: a *suspended* session is motionless on
/// purpose — and `apply_ahead_window` restarts the motion clock at resume, so
/// the suspension itself can never be read back as a stall — and a *cached*
/// session has no process at all, so there is nothing here to watch.
async fn watch_for_stall(session: Arc<Session>, dir: PathBuf, sid: String) {
    // No process, nothing to judge — and `failed` on a cache entry would be a
    // lie with consequences (its segments exist; readers would refuse them).
    // No start path spawns a watchdog for a cache hit; this is the guard that
    // keeps that true if one ever does.
    if session.cached {
        return;
    }
    // A generous floor before the first verdict: opening a 4K file over NFS
    // and filling a first segment is legitimately slow, and `stalled_for`
    // counts from session start, so judging earlier would fail cold opens.
    tokio::time::sleep(SOFTWARE_GRACE).await;
    let mut produced = false;
    loop {
        // Once true, stays true — and stops the per-poll playlist read. The
        // flag only picks the failure message: which half of the session's
        // life wedged decides what the operator should go look at.
        if !produced && session_producing(&dir).await {
            produced = true;
        }
        let exited = {
            let mut child = session.child.lock().await;
            child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
        };
        match watch_next(
            session.failed.load(Relaxed),
            exited,
            session.suspended.load(Relaxed),
            session.progress.stalled_for(),
        ) {
            WatchNext::Done => return,
            WatchNext::Wait => tokio::time::sleep(WATCHDOG_POLL).await,
            WatchNext::Stall => {
                tracing::error!(
                    session = %sid,
                    stalled_s = session.progress.stalled_for().as_secs(),
                    produced_ms = session.progress.out_time_ms(),
                    "{}",
                    if produced {
                        "transcode output stopped advancing mid-stream; failing the session \
                         so the player can recover instead of draining its buffer into a hang"
                    } else {
                        "transcode produced no playable segment and its output has stopped \
                         advancing; failing the session — the source is likely undecodable \
                         by this ffmpeg build (e.g. a Dolby Vision profile it can't handle)"
                    }
                );
                session.kill_child().await;
                session.failed.store(true, Relaxed);
                return;
            }
        }
    }
}

/// Remove the (empty/partial) HLS output so a restarted ffmpeg starts clean.
async fn clear_session_dir(dir: &std::path::Path) {
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Fail in terms of the dependency that is actually missing.
///
/// Several tests here and in [`crate::http`] drive the real spawn path. They
/// don't need ffmpeg to *succeed* — the fixtures are placeholder bytes, so it
/// always exits with an error — they need it to *start*, because what they
/// assert on is the session bookkeeping that only exists once there is a child
/// process to track. Absent ffmpeg that arrives as `No such file or directory
/// (os error 2)` under twenty frames of tokio, or, having been through the HTTP
/// layer first, as nothing more informative than `left: 500, right: 200`.
///
/// plurxd shells out to ffmpeg at runtime, so this is a dependency to install
/// rather than a test to skip: skipping would let CI report green on the
/// transcode paths without having run any of them.
#[cfg(test)]
pub(crate) fn require_ffmpeg() {
    let bin = ffmpeg_bin();
    if let Err(err) = std::process::Command::new(&bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        panic!(
            "this test needs ffmpeg, and running `{bin}` failed: {err}\n\
             install it (`apt-get install ffmpeg`, `brew install ffmpeg`) or point \
             PLURX_FFMPEG at a build — plurxd requires it at runtime too"
        );
    }
}

fn tone_map_pref() -> ToneMap {
    match std::env::var("PLURX_TONEMAP").as_deref() {
        Ok("libplacebo") => ToneMap::Libplacebo,
        Ok("off" | "none" | "passthrough") => ToneMap::None,
        _ => ToneMap::Zscale,
    }
}

struct LastRequest {
    at: Instant,
    kind: &'static str,
}

impl LastRequest {
    fn now(kind: &'static str) -> Self {
        Self {
            at: Instant::now(),
            kind,
        }
    }
}

struct Session {
    dir: PathBuf,
    /// The ffmpeg producing this session's segments — `None` for a cache hit,
    /// where the segments already exist and there is nothing to run, watch,
    /// suspend or kill.
    child: Mutex<Option<Child>>,
    /// Served from the pre-transcode cache.
    ///
    /// Two things follow, and the second is the dangerous one. A cached
    /// session has no process, so every watchdog, stall check and flow-control
    /// decision is about nothing. And its directory is a finished asset this
    /// session did not produce and other viewers will want — deleting it on
    /// the way out, which is what every other session does, would destroy the
    /// cache one playback at a time.
    cached: bool,
    /// Process-local read ownership for a finished cache entry. Kept for the
    /// session's full lifetime so the budget sweep cannot remove its playlist
    /// or segments while an HTTP response can still reach them.
    _cache_reader: Option<crate::cachekeep::CacheReadGuard>,
    /// The request that keeps this session alive. Keeping the kind beside the
    /// clock makes an idle reap explain whether the last sign of life was a
    /// playlist reload, a media segment, or only a subtitle/context lookup.
    last_request: Mutex<LastRequest>,
    // -- metadata for the activity page --
    file_id: i64,
    item_id: i64,
    item_title: String,
    user_name: String,
    /// The player instance that owns this session — the supersession key.
    playback_id: String,
    /// Re-encoding the picture, or only repackaging it. Immutable, unlike
    /// `encoder_label`: what this session *is* does not change when the
    /// encoder behind it does, and the activity page must not relabel a copy
    /// as a transcode because a fallback fired or a cache hit answered.
    method: crate::delivery::Method,
    /// Where this session's timeline begins in the source, so a recovered
    /// (idempotent) create reports the same offset the first one did.
    start_seconds: f64,
    /// Where the session's media *actually* begins in the source, which is
    /// not always what was asked for. A transcode seeks accurately and starts
    /// exactly at `start_seconds`; a copy session seeks with
    /// `-noaccurate_seek` and therefore begins at the keyframe before it, up
    /// to a full GOP earlier. Anything that maps a source timestamp onto this
    /// session's timeline has to subtract THIS, not the request — subtitle
    /// cues did the latter and led the picture by up to six seconds on 4K
    /// film GOPs.
    media_origin_seconds: f64,
    /// RFC 6381 sample types present in the primary HLS rendition. A native
    /// subtitle master must advertise these; omitting or abbreviating the
    /// referenced formats makes AVPlayer reject an otherwise playable copy.
    hls_codecs: String,
    /// Backward-compatible enhancement carried by the video samples, such as
    /// Dolby Vision Profile 8.1 over an HDR10 HEVC base layer. Apple requires
    /// this outside CODECS so clients that only understand the base can still
    /// select the variant.
    hls_supplemental_codecs: Option<String>,
    target_height: i64,
    /// The encoder actually running *now*. Mutable because the
    /// hardware->software fallback replaces the process inside one session,
    /// and an activity page still naming the hardware encoder after that is
    /// reporting a pipeline that no longer exists.
    encoder_label: Mutex<&'static str>,
    started_unix: i64,
    /// Set when the session can never produce output (hardware and software
    /// both failed to emit a first segment). Playlist/segment reads then fail
    /// fast so the player shows an error instead of waiting on a gray screen.
    failed: AtomicBool,
    /// True once the first playlist response is allowed out. Cached assets and
    /// copy sessions start true: VOD is already complete, and the copy
    /// segmenter owns its separate 12-second publication gate. A live
    /// transcode flips this exactly once when its small startup cushion exists.
    playlist_published: AtomicBool,
    /// Highest segment index the client has fetched (-1 before the first).
    /// Kept for logs and for resolving the frontier against the index; the
    /// accounting itself works in media time.
    high_segment: AtomicI64,
    /// The client's DOWNLOAD frontier in session-relative ms: the end of the
    /// furthest segment served, from that segment's own `EXTINF`. Not the
    /// playhead — a client fetches its whole forward buffer ahead of the
    /// picture — and every name and log line here says so.
    fetched_end_ms: AtomicI64,
    /// What the playlist says is published, refreshed as segments complete.
    segments: Mutex<SegmentIndex>,
    /// Published bytes past the client's frontier, cached from the last
    /// refresh. This is the PACING number: how much reserve the client has.
    /// It is not a disk number — retention keeps [`RETENTION_SECS`] of
    /// media *behind* the frontier too, and those bytes are just as much on
    /// the disk (review §2.7).
    ahead_bytes: AtomicI64,
    /// Everything this session has on disk that retention has not deleted —
    /// ahead of the frontier and behind it alike. This is the BUDGET number:
    /// what the global scratch cap sums. The two used to be one figure, and
    /// the cap it produced was not a bound: several healthy sessions could
    /// exceed the documented ceiling by their whole retention windows.
    live_bytes: AtomicI64,
    /// Live encode telemetry (see [`Progress`]).
    progress: Arc<Progress>,
    /// What kind of work this is, for the admission record (see
    /// [`crate::admission::Workload::class`]). Kept on the session because the
    /// speed that matters is measured while it runs, long after the file that
    /// described it went out of scope. Mutable because the hardware→software
    /// fallback replaces the encoder inside one session — speeds measured
    /// after that are software speeds, and recording them under the hardware
    /// class would poison the very measurements admission decides by.
    class: std::sync::Mutex<String>,
    /// The hardware slot this session holds.
    ///
    /// Released two ways, on purpose. `release_hardware` hands it back the
    /// moment the session ends, which is what makes the cap *prompt*: the
    /// watchdog task holds an `Arc` to this session for its whole grace window,
    /// so waiting for the last reference to go would keep a slot for twelve
    /// seconds after the viewer closed the tab. And dropping the session
    /// returns it too, which is what makes the cap *complete* — every way a
    /// session can end, including the ones nobody wrote a branch for.
    hw_slot: std::sync::Mutex<Option<HwSlot>>,
    /// This session's reservation from the software CPU pool, when it runs
    /// (or fell back to) a software encoder. Held for the session's life and
    /// released the same two ways as the hardware slot, for the same two
    /// reasons: promptly via `release_software`, completely via drop.
    sw_permit: std::sync::Mutex<Option<crate::admission::SwPermit>>,
    /// Bytes of segment actually handed to this client, and how fast.
    ///
    /// The player cannot measure this for itself on every transport: native
    /// HLS (Safari's HEVC path) exposes no such number, and hls.js's estimator
    /// exists only when hls.js is the one fetching. The server serves every
    /// segment on every path, so it is the one place the answer always exists.
    delivery: Meter,
    /// Effective input pace for this session; 0 means unpaced.
    readrate: f64,
    /// True while the child is SIGSTOPped for running too far ahead of the
    /// playhead. Everything that judges a session's health has to know: a
    /// suspended encoder makes no progress *on purpose*.
    suspended: AtomicBool,
    /// When the current held interval began, for the resume event's duration.
    suspended_at: Mutex<Option<(Instant, AheadHoldReason)>>,
    /// Successful running→held transitions during this session. A counter,
    /// rather than only the current boolean, exposes flapping after it has
    /// already resumed.
    suspend_count: AtomicU64,
    /// Snapshot of the EVENT-to-typeless experiment at session creation. A
    /// settings edit must never mutate one URL's playlist type mid-play.
    typeless_sliding: bool,
    /// The first retained-prefix advance gets one operational log line. A
    /// playlist reload may observe that state hundreds of times; only the
    /// transition is evidence about the EVENT/sliding experiment.
    first_slide_logged: AtomicBool,
}

#[derive(Default)]
struct SessionEventFields<'a> {
    reason: Option<&'a str>,
    extra: Option<String>,
    hold_reason: Option<AheadHoldReason>,
    ms: Option<i64>,
}

/// How far a session's published media runs ahead of the client's download
/// frontier, in seconds and in bytes.
///
/// Both terms are session-relative: ffmpeg's input seek restarts output
/// timestamps at zero and segment numbering restarts with the session, so the
/// subtraction needs no absolute timeline.
fn ahead_of(index: &SegmentIndex, fetched_end_ms: i64) -> Option<Ahead> {
    let produced_end = index.produced_playable_end_ms()?;
    Some(Ahead {
        seconds: (produced_end - fetched_end_ms) / 1000,
        bytes: index.bytes_after_ms(fetched_end_ms),
    })
}

impl Session {
    /// Stop the encoder, if there is one. A cache hit has no process; a
    /// session whose ffmpeg already exited has one that is already reaped.
    async fn kill_child(&self) {
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.kill().await;
        }
    }

    /// Remove this session's scratch — unless it is a cache entry, which this
    /// session did not produce and the next viewer still wants.
    ///
    /// The guard is the whole reason this is a method. Every other place a
    /// session ends removes its directory, correctly, and a cached session
    /// reaching one of those paths without this check would delete the cache
    /// one playback at a time — each hit destroying the entry that made it.
    async fn discard_dir(&self) {
        if self.cached {
            return;
        }
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }

    /// Hand the hardware slot back now rather than whenever the last reference
    /// to this session happens to go. Idempotent, because the reaper and an
    /// explicit stop can both reach a session and neither should have to know
    /// whether the other got there first.
    fn release_hardware(&self) {
        let _ = self.hw_slot.lock().expect("hw slot mutex").take();
    }

    /// Hand the software pool its threads back now — the watchdog task holds
    /// an `Arc` to this session for its whole grace window, and a viewer who
    /// closed the tab should not keep cores reserved for it.
    fn release_software(&self) {
        let _ = self.sw_permit.lock().expect("sw permit mutex").take();
    }

    /// The bookkeeping half of the hardware→software fallback, split out so a
    /// test can prove it. Two things must happen at the transition, not at
    /// teardown: the hardware slot goes back (a software session holding a
    /// GPU slot parks the next hardware start in the admission queue for as
    /// long as this session lives — potentially a whole film), and the
    /// admission class flips to software, so the speeds measured from here on
    /// are recorded as what they are rather than poisoning the hardware
    /// class's record with a software encoder's numbers.
    fn demote_to_software(&self, work: Workload<'_>, permit: crate::admission::SwPermit) {
        self.release_hardware();
        *self.class.lock().expect("class mutex") = work.software_class();
        // Forced, not negotiated — the viewer is already watching — but on
        // the books: the pool runs over budget and every later admission
        // sees it (review §2.4).
        *self.sw_permit.lock().expect("sw permit mutex") = Some(permit);
    }

    /// Re-read the playlist, take in what is newly published, and measure
    /// only that.
    ///
    /// The index is extended in place ([`SegmentIndex::extend_from_playlist`])
    /// rather than reconstructed, sizes stay where they were measured, and a
    /// pruned segment is never re-stated — this runs on every segment publish
    /// and frontier advance, and it used to redo a whole session's worth of
    /// parsing and ENOENTs each time (review §2.6).
    async fn refresh_segments(&self) {
        let Ok(raw) = tokio::fs::read(self.dir.join("index.m3u8")).await else {
            return;
        };
        // Under the lock only to extend; the stats happen with it released.
        let to_stat: Vec<(i64, String)> = {
            let mut index = self.segments.lock().await;
            if index.extend_from_playlist(&String::from_utf8_lossy(&raw)) {
                tracing::debug!("segment index rebuilt — the playlist was truncated or replaced");
            }
            index
                .segs
                .iter()
                .filter(|s| s.bytes == 0 && !s.pruned)
                .map(|s| (s.index, s.name.clone()))
                .collect()
        };
        let mut sizes: Vec<(i64, i64)> = Vec::with_capacity(to_stat.len());
        for (idx, name) in to_stat {
            if let Ok(meta) = tokio::fs::metadata(self.dir.join(&name)).await {
                sizes.push((idx, meta.len() as i64));
            }
        }
        let mut index = self.segments.lock().await;
        for (idx, len) in sizes {
            if let Some(s) = index.segs.iter_mut().find(|s| s.index == idx && !s.pruned) {
                s.bytes = len;
            }
        }
        // Resolve the frontier against the fresh index: a segment served
        // before its EXTINF was known gets its real end time now.
        let high = self.high_segment.load(Relaxed);
        if high >= 0 {
            if let Some(end) = index.end_ms_of(high) {
                self.fetched_end_ms.fetch_max(end, Relaxed);
            }
        }
        // A cached asset's bytes are not scratch, and must not be counted as
        // any. The global budget is a sum over every session, and it decides
        // whether *live* encoders get suspended — so a 6 GB cached 4K title
        // reported here would blow the budget the moment somebody pressed
        // play and hold every real encoder on the box. Those bytes are already
        // accounted for, by the cache's own size budget.
        if !self.cached {
            if let Some(ahead) = ahead_of(&index, self.fetched_end_ms.load(Relaxed).max(0)) {
                self.ahead_bytes.store(ahead.bytes, Relaxed);
            }
            self.live_bytes.store(index.total_bytes(), Relaxed);
        }
    }

    async fn ahead(&self) -> Option<Ahead> {
        ahead_of(
            &*self.segments.lock().await,
            self.fetched_end_ms.load(Relaxed).max(0),
        )
    }
}

/// One live session as the activity page and the stats overlay see it.
async fn session_info(
    id: &str,
    s: &Session,
    limits: AheadLimits,
    global_live_bytes: i64,
    global_ahead_bytes: i64,
) -> SessionInfo {
    let ahead = s.ahead().await;
    let suspended = s.suspended.load(Relaxed);
    let hold = if suspended {
        ahead.and_then(|ahead| {
            ahead_hold(ahead, global_live_bytes, global_ahead_bytes, limits, true)
        })
    } else {
        None
    };
    SessionInfo {
        id: id.to_owned(),
        file_id: s.file_id,
        item_id: s.item_id,
        item_title: s.item_title.clone(),
        user_name: s.user_name.clone(),
        target_height: s.target_height,
        encoder: *s.encoder_label.lock().await,
        started_unix: s.started_unix,
        idle_seconds: s.last_request.lock().await.at.elapsed().as_secs(),
        speed: s.progress.speed(),
        recent_speed: s.progress.recent_speed(),
        out_time_ms: s.progress.out_time_ms(),
        ahead_seconds: ahead.map(|a| a.seconds),
        hold_reason: hold.map(|hold| hold.reason),
        resume_below_seconds: hold
            .filter(|hold| hold.reason == AheadHoldReason::Time)
            .map(|hold| hold.release_value),
        resume_below_bytes: hold
            .filter(|hold| hold.reason != AheadHoldReason::Time)
            .map(|hold| hold.release_value),
        ahead_bytes: ahead.map(|a| a.bytes),
        delivered_bytes: s.delivery.total_bytes(),
        delivered_bps: s.delivery.recent_bps().map(|b| b * 8),
        delivered_idle_ms: s.delivery.idle_for_ms(),
        readrate: s.readrate,
        suspended,
        suspend_count: s.suspend_count.load(Relaxed),
    }
}

/// A segment, open and ready to stream.
pub struct SegmentFile {
    pub file: tokio::fs::File,
    pub len: u64,
}

/// The source timeline behind one capability-authenticated HLS session.
/// Subtitle child requests resolve this instead of accepting a file id from
/// the URL, so one session capability can never be used to read another file.
///
/// `codecs` and `supplemental_codecs` describe the exact formats in the
/// session's primary rendition. The native Apple master uses them only for
/// HDR variants: Apple requires the exact Main10/Dolby declaration alongside
/// `VIDEO-RANGE`, while leaving SDR masters codec-neutral preserves the broad
/// compatibility established by physical-device testing.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsContext {
    pub file_id: i64,
    pub start_seconds: f64,
    /// The source timestamp that this session's media calls t=0. See
    /// `Session::media_origin_seconds` — subtitle cue shifting uses this, and
    /// using `start_seconds` instead is the P0-2 defect.
    pub media_origin_seconds: f64,
    pub codecs: String,
    pub supplemental_codecs: Option<String>,
    /// Maximum video frame rate from the source probe. Apple requires this on
    /// every video variant in a multivariant playlist.
    pub frame_rate: Option<f64>,
}

/// How long a media-origin probe may take before the session gives up on it.
///
/// This runs on the session-creation path, so it is in front of the viewer.
/// A seek plus four packets is milliseconds on any healthy source; a source
/// that cannot answer that quickly is a source whose session is about to have
/// much larger problems, and the fallback (the requested start) is exactly
/// what this code used unconditionally before.
const MEDIA_ORIGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a copy session's media actually begins in the source.
///
/// See [`plurx_core::transcode::keyframe_probe_args`] for why the requested
/// start is not the answer. Every failure path returns `start_seconds`, which
/// is both the old behaviour and the correct answer whenever the requested
/// offset happens to land on a keyframe.
pub(crate) async fn probe_media_origin(source_path: &std::path::Path, start_seconds: f64) -> f64 {
    if start_seconds <= 0.0 {
        return 0.0;
    }
    let args =
        plurx_core::transcode::keyframe_probe_args(&source_path.to_string_lossy(), start_seconds);
    let probe = tokio::process::Command::new(crate::ffmpeg::ffprobe_bin())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(MEDIA_ORIGIN_PROBE_TIMEOUT, probe).await else {
        tracing::warn!(
            start_seconds,
            "media-origin probe did not answer; subtitle cues fall back to the requested start"
        );
        return start_seconds;
    };
    if !out.status.success() {
        tracing::warn!(
            start_seconds,
            "media-origin probe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return start_seconds;
    }
    let origin =
        plurx_core::transcode::parse_keyframe_origin(&String::from_utf8_lossy(&out.stdout))
            // A keyframe *after* the requested start would mean the demuxer
            // seeked forward, which `-noaccurate_seek` does not do. Treat it as a
            // probe that misread rather than moving cues the wrong way.
            .filter(|origin| *origin <= start_seconds + 0.001)
            .unwrap_or(start_seconds);
    if (origin - start_seconds).abs() > 0.05 {
        tracing::info!(
            start_seconds,
            origin,
            lead_seconds = start_seconds - origin,
            "copy session begins at the preceding keyframe; subtitle cues shift by the origin"
        );
    }
    origin
}

fn audio_track(
    file: &plurx_core::domain::MediaFile,
    selected: Option<i64>,
) -> Option<&plurx_core::domain::AudioStream> {
    selected
        .and_then(|index| file.audio_streams.iter().find(|track| track.index == index))
        .or_else(|| file.audio_streams.iter().find(|track| track.default))
        .or_else(|| file.audio_streams.first())
}

/// RFC 6381 sample type for audio that is being *copied* into the HLS
/// rendition.
///
/// The `_` arm answers `mp4a.40.2` (AAC-LC) for anything unlisted, which
/// **mislabels a genuinely copied FLAC, Opus or DTS track** — it claims AAC
/// for bytes that are not AAC. The Apple client only ever claims codecs the
/// arms above cover, but the web player claims `flac`/`opus` when the browser
/// does, and Safari takes the copy-HLS path — so a FLAC-in-MKV remux reaches
/// this arm today. Recorded rather than fixed
/// (CLIENTS-REMEDIATION-PLAN §8.3) because the value has no consumer at all:
/// the native master carries no `CODECS` attribute (see [`HlsContext`]), so
/// the wrong label is currently written to nothing. It is one match arm, and
/// it wants doing in the same change as whatever starts reading the result —
/// a fix landed now would be untestable through any wire output.
fn copied_audio_codec(file: &plurx_core::domain::MediaFile, selected: Option<i64>) -> &'static str {
    match audio_track(file, selected).map(|track| track.codec.as_str()) {
        Some("ac3" | "ac-3") => "ac-3",
        Some("eac3" | "eac-3" | "ec-3") => "ec-3",
        Some("alac") => "alac",
        Some("mp3") => "mp4a.40.34",
        _ => "mp4a.40.2",
    }
}

fn copied_hls_codecs(
    file: &plurx_core::domain::MediaFile,
    audio_index: Option<i64>,
    options: CopySessionOptions,
    probe_json: Option<&str>,
) -> (String, Option<String>) {
    let mut supplemental = None;
    let video = match file.video_codec.as_deref() {
        Some("hevc" | "h265")
            if options.preserve_dolby_vision && file.hdr.as_deref() == Some("dolby_vision") =>
        {
            match dolby_vision_hls_config(probe_json) {
                Some(config) if config.profile == 8 && config.compatibility_id == Some(1) => {
                    supplemental = Some(format!("{}/db1p", config.codec));
                    hevc_hls_codec(probe_json).unwrap_or_else(|| "hvc1".to_owned())
                }
                Some(config) if config.profile == 8 && config.compatibility_id == Some(4) => {
                    supplemental = Some(format!("{}/db4h", config.codec));
                    hevc_hls_codec(probe_json).unwrap_or_else(|| "hvc1".to_owned())
                }
                Some(config) => config.codec,
                None => "dvh1".to_owned(),
            }
        }
        Some("hevc" | "h265") => hevc_hls_codec(probe_json).unwrap_or_else(|| "hvc1".to_owned()),
        Some("h264" | "avc") => {
            avc_hls_codec(probe_json).unwrap_or_else(|| "avc1.640034".to_owned())
        }
        _ => "avc1.640034".to_owned(),
    };
    let audio = if options.transcode_audio {
        "mp4a.40.2"
    } else {
        copied_audio_codec(file, audio_index)
    };
    (format!("{video},{audio}"), supplemental)
}

/// AVPlayer accepts a media playlist without codec metadata, but a
/// multivariant playlist has to describe Dolby Vision with its RFC 6381
/// profile and level. A bare `dvh1` makes an otherwise playable copied stream
/// fail during asset preparation. The scanner keeps ffprobe's DOVI
/// configuration record verbatim, so use the exact values that are also
/// carried by the remuxed sample entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DolbyVisionHlsConfig {
    codec: String,
    profile: u64,
    compatibility_id: Option<u64>,
}

fn video_probe_stream(probe_json: Option<&str>) -> Option<serde_json::Value> {
    let probe: serde_json::Value = serde_json::from_str(probe_json?).ok()?;
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|value| value.as_str()) == Some("video"))
        .cloned()
}

fn dolby_vision_hls_config(probe_json: Option<&str>) -> Option<DolbyVisionHlsConfig> {
    let video = video_probe_stream(probe_json)?;
    let dovi = video
        .get("side_data_list")?
        .as_array()?
        .iter()
        .find(|side| {
            side.get("side_data_type")
                .and_then(|value| value.as_str())
                .is_some_and(|kind| kind.contains("DOVI") || kind.contains("Dolby Vision"))
        })?;
    let profile = dovi.get("dv_profile")?.as_u64()?;
    let level = dovi.get("dv_level")?.as_u64()?;
    Some(DolbyVisionHlsConfig {
        codec: format!("dvh1.{profile:02}.{level:02}"),
        profile,
        compatibility_id: dovi
            .get("dv_bl_signal_compatibility_id")
            .and_then(serde_json::Value::as_u64),
    })
}

/// RFC 6381 HEVC identifier for the HDR10/HLG base layer used by a
/// backward-compatible Dolby Vision stream. The sources plurx copies are
/// Main/Main10, and ffprobe's numeric `level` is already the value RFC 6381
/// places after the tier letter (for example 150 for HEVC level 5.0).
fn hevc_hls_codec(probe_json: Option<&str>) -> Option<String> {
    let video = video_probe_stream(probe_json)?;
    let (profile, compatibility) = match video.get("profile")?.as_str()? {
        // RFC 6381 writes HEVC compatibility flags in reverse bit order.
        // Apple's HLS appendix consequently spells these `1.6` and `2.4`,
        // not the raw ffprobe/HEVC bit masks `60000000` and `20000000`.
        "Main" => (1, "6"),
        "Main 10" => (2, "4"),
        _ => return None,
    };
    let level = video.get("level")?.as_u64()?;
    let tier = if video.get("tier").and_then(serde_json::Value::as_str) == Some("High") {
        'H'
    } else {
        'L'
    };
    Some(format!("hvc1.{profile}.{compatibility}.{tier}{level}.B0"))
}

fn avc_hls_codec(probe_json: Option<&str>) -> Option<String> {
    let video = video_probe_stream(probe_json)?;
    let profile = match video.get("profile")?.as_str()? {
        "Baseline" | "Constrained Baseline" => 66_u8,
        "Main" => 77,
        "High" => 100,
        _ => return None,
    };
    let level = video.get("level")?.as_u64()?;
    let level = u8::try_from(level).ok()?;
    Some(format!("avc1.{profile:02x}00{level:02x}"))
}

pub struct StartInfo {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
    /// The source timestamp represented by player-local time zero.
    ///
    /// Copy sessions may begin at the keyframe before `start_seconds`; an
    /// accurate transcode begins at the requested position, and a cached VOD
    /// begins at zero. Clients use this value for progress and timed overlays.
    pub media_origin_seconds: f64,
    pub encoder: &'static str,
    /// Served from the cache: every segment already exists, so this is a VOD
    /// asset rather than a stream being written.
    ///
    /// The player needs to know, because the difference is visible. A live
    /// session seeks by restarting the encoder somewhere else; a finished one
    /// seeks by moving `currentTime`, like direct play, in well under a second.
    /// And the stall watchdog's restart arm has nothing to fix here — a
    /// segment that is late was never going to be produced faster.
    pub vod: bool,
}

/// A live session, as the activity page sees it.
#[derive(Clone, serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub file_id: i64,
    pub item_id: i64,
    pub item_title: String,
    pub user_name: String,
    pub target_height: i64,
    pub encoder: &'static str,
    pub started_unix: i64,
    pub idle_seconds: u64,
    /// Cumulative encode rate as a multiple of realtime, as ffmpeg reports it.
    pub speed: Option<f64>,
    /// Rate over the last few seconds. This is the one that answers "is the
    /// server keeping up *now*" — the cumulative figure hides a slowdown
    /// behind a fast start, which is exactly the shape a session takes when
    /// its burst has ended and the encoder can't hold realtime.
    pub recent_speed: Option<f64>,
    /// Content produced so far, in ms from this session's start offset.
    pub out_time_ms: Option<i64>,
    /// Published media beyond the client's download frontier — the reserve a
    /// hiccup gets to spend. Not measured from the playhead: the client has
    /// usually fetched further than it is showing.
    pub ahead_seconds: Option<i64>,
    /// The active limit keeping this session held. Capacity reasons take
    /// precedence when more than one limit is above its release point.
    pub hold_reason: Option<AheadHoldReason>,
    /// Present only for a time-held session.
    pub resume_below_seconds: Option<i64>,
    /// Present only for a per-session or global byte hold. For `global`, this
    /// is the release point for total live scratch across every session.
    pub resume_below_bytes: Option<i64>,
    /// The same reserve in bytes, which is what actually bounds the disk.
    pub ahead_bytes: Option<i64>,
    /// Segment bytes handed to this client since the session opened.
    pub delivered_bytes: i64,
    /// Recent delivery rate in BITS per second, or `None` before a window has
    /// closed. Measured here rather than in the player because only the server
    /// sees every transport — Safari's native HLS reports no such number, and
    /// hls.js's estimator exists only when hls.js is doing the fetching.
    pub delivered_bps: Option<i64>,
    /// How long since that rate was last recomputed. A client with a full
    /// buffer stops fetching, which is health, not a slow link — the rate keeps
    /// its last real value and this says how old it is, so a reader can tell a
    /// measurement from a memory.
    pub delivered_idle_ms: i64,
    /// Effective ffmpeg input pace, matching `StreamInfo.readrate`.
    pub readrate: f64,
    pub suspended: bool,
    /// Number of times this producer entered a held state. This stays visible
    /// after resume so a flapping session cannot look healthy merely because
    /// the activity poll landed between transitions.
    pub suspend_count: u64,
}

/// What a client asked for, normalised. Two requests with the same
/// fingerprint would produce byte-identical output, which is what makes a
/// repeated create safe to answer with the session that already exists.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub file_id: i64,
    /// Stable for one player instance; the supersession key.
    pub playback_id: String,
    /// Optional idempotency key for one creation attempt.
    pub request_id: Option<String>,
    pub kind: SessionKind,
    pub start_seconds: f64,
    pub audio_index: Option<i64>,
    /// Subtitle stream to burn into the picture, chosen by the viewer.
    ///
    /// Only ever a *burn*: a text subtitle the client can render itself never
    /// comes through here — it fetches the VTT and shows it locally, which
    /// costs the server nothing and can be toggled without restarting a
    /// stream. This is for the ones that have no other way in, above all the
    /// PGS tracks a UHD Blu-ray remux carries and which plurx simply had no
    /// answer for before.
    pub subtitle_burn: Option<i64>,
    /// Manual A/V correction for this playback only (positive delays audio).
    pub audio_offset_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionKind {
    Transcode {
        height: i64,
    },
    /// Copy the source video; `aac` re-encodes the audio the client can't take.
    Copy {
        aac: bool,
        preserve_dolby_vision: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct CopySessionOptions {
    transcode_audio: bool,
    preserve_dolby_vision: bool,
}

impl SessionRequest {
    /// A stable description of the output this request would produce. Start
    /// position is included: two creates at different offsets are different
    /// streams, and answering the second with the first would silently seek
    /// the viewer somewhere they didn't ask to be.
    fn fingerprint(&self) -> String {
        let kind = match self.kind {
            SessionKind::Transcode { height } => format!("t{height}"),
            SessionKind::Copy {
                aac,
                preserve_dolby_vision,
            } => format!("c{}d{}", u8::from(aac), u8::from(preserve_dolby_vision)),
        };
        format!(
            "{}:{kind}:{:.3}:{}:{}:{}",
            self.file_id,
            self.start_seconds,
            self.audio_index.unwrap_or(-1),
            self.subtitle_burn.unwrap_or(-1),
            self.audio_offset_ms
        )
    }
}

/// One creation request's lifecycle, keyed by the client's `request_id`.
enum RequestState {
    /// A create with this id is running and its outcome isn't known yet.
    /// A concurrent create with the same id waits for it rather than pass
    /// the same check and start a second encoder.
    InFlight,
    /// The create finished, and this is the session it made.
    Ready(String),
}

/// How long a create will wait on an identical in-flight one before calling
/// it lost. An honest peer resolves within the slot queue's patience plus
/// spawn overhead; a reservation still standing past this belongs to a
/// process that died without unwinding, and erroring beats hanging the
/// player behind it.
const INFLIGHT_WAIT: Duration = Duration::from_secs(QUEUE_WAIT.as_secs() + 10);
const INFLIGHT_POLL: Duration = Duration::from_millis(100);

/// The reservation a creating call holds while it works.
///
/// Exists so a create that never completes — an error, or a caller that
/// vanished mid-await (a closed tab drops its request future wherever it
/// happens to be) — cannot leave its `request_id` parked `InFlight` forever,
/// wedging every honest retry behind [`INFLIGHT_WAIT`]. `complete` records
/// the session and defuses the guard; `Drop` covers every other exit by
/// clearing the reservation so the next attempt starts fresh.
struct RequestClaim<'a> {
    requests: &'a std::sync::Mutex<HashMap<String, (String, RequestState)>>,
    key: Option<String>,
    fingerprint: String,
}

impl RequestClaim<'_> {
    /// Record the created session under this reservation's key, pruning
    /// entries whose sessions have ended so the map cannot grow for the life
    /// of the process. In-flight reservations are never pruned — their
    /// sessions aren't in the map *yet*.
    fn complete(mut self, session_id: &str, live: &std::collections::HashSet<String>) {
        let Some(key) = self.key.take() else { return };
        let mut requests = self.requests.lock().expect("requests mutex");
        requests.retain(|_, (_, state)| match state {
            RequestState::InFlight => true,
            RequestState::Ready(sid) => live.contains(sid),
        });
        requests.insert(
            key,
            (
                std::mem::take(&mut self.fingerprint),
                RequestState::Ready(session_id.to_owned()),
            ),
        );
    }
}

impl Drop for RequestClaim<'_> {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        if let Ok(mut requests) = self.requests.lock() {
            // Only this claim's own reservation. Nothing else removes an
            // `InFlight` entry, so finding one under our key means it is
            // ours; a `Ready` entry is a completed create and belongs to
            // the map.
            if matches!(requests.get(&key), Some((_, RequestState::InFlight))) {
                requests.remove(&key);
            }
        }
    }
}

/// What claiming a `request_id` resolved to.
enum Claimed<'a> {
    /// This call owns the create; the claim must be completed or dropped.
    Mine(RequestClaim<'a>),
    /// An identical create already made this session.
    Recovered(StartInfo),
}

/// How often a running producer checks whether a viewer wants its slot.
///
/// Short, because this interval *is* the latency a viewer pays to preempt it:
/// a quarter-second of polling plus a kill is well inside the five seconds a
/// live start is willing to queue, and the poll itself costs nothing.
const PRODUCER_POLL: Duration = Duration::from_millis(250);

/// How long a producer waits before asking for a slot again after being
/// refused one. Longer than the poll: it has already been told a viewer is
/// there, and retrying eagerly would just spin.
const PRODUCER_RETRY: Duration = Duration::from_secs(5);

/// The producer timings that are data rather than constants, so a test can
/// state what it needs instead of racing the hardware.
///
/// Every field defaults to the production value above and nothing in the
/// daemon ever changes one — `TranscodeManager::new` takes the default and
/// there is no setter outside `#[cfg(test)]`. They live here because the
/// resume path is only reachable by *interrupting an encoder that is still
/// running*, and whether that is possible at all depends on how fast the box
/// is: a 16-core desktop finished the whole 240-second fixture between two
/// sleeps that a 2-core CI runner needed five seconds for, so the test
/// asserting "this was preempted" was really asserting "this machine is
/// slow". Pacing the producer's input makes a part's wall-clock duration a
/// property of the *source and the rate* instead of the CPU, and shortening
/// the retry keeps the test from paying five seconds per preemption.
#[derive(Debug, Clone, Copy)]
struct ProducerTuning {
    /// Pacing for a producer part's input. [`Pacing::unpaced`] in production —
    /// see the comment at the `hls_args` call in `produce_into` for why.
    pacing: Pacing,
    /// Stand-in for [`PRODUCER_RETRY`].
    retry: Duration,
}

impl Default for ProducerTuning {
    fn default() -> Self {
        ProducerTuning {
            pacing: Pacing::unpaced(),
            retry: PRODUCER_RETRY,
        }
    }
}

/// How many times one run will resume after being preempted before giving up
/// until the next producer pass.
///
/// A bound rather than a timeout because the failure it guards against is not
/// slowness but *thrash*: a busy evening where every part is killed within
/// seconds would otherwise spend the whole night starting encoders and
/// throwing them away. Progress is kept either way — the next pass resumes
/// from the same boundary.
const PRODUCER_MAX_PARTS: usize = 64;

/// The recipe format this build writes into new claims. Stored per row so a
/// future change to what a recipe *means* can be recognised rather than
/// guessed at.
const CACHE_RECIPE_VERSION: i64 = 1;

/// Why a producer part stopped.
#[derive(Debug)]
enum PartEnd {
    /// ffmpeg reached the end of the file.
    Finished,
    /// A viewer wants the hardware.
    Preempted,
    /// This producer run is out of time.
    Deadline,
    Failed(String),
}

/// Which tracks a session carries. Part of its recipe, which is why it is a
/// named thing rather than two loose values.
#[derive(Debug, Clone, Default)]
struct Tracks {
    audio_index: Option<i64>,
    subtitle_burn: Option<plurx_core::transcode::SubtitleBurn>,
}

/// A published cache entry, as measured on disk.
#[derive(Debug, Clone, Copy)]
struct Published {
    bytes: i64,
    duration_ms: i64,
    segments: usize,
    /// How many separate encoder runs it took — one, plus one per preemption.
    ///
    /// Worth reporting rather than inferring: it is the only number that says
    /// how contended the box was while this was made, and it is the thing a
    /// test of the resume path has to assert on, or that test passes on a
    /// fixture small enough to finish before it is ever interrupted.
    parts: usize,
}

/// What one `produce` call achieved.
#[derive(Debug, Clone)]
pub struct Produced {
    pub recipe: String,
    pub bytes: i64,
    pub duration_ms: i64,
    pub segments: usize,
    pub parts: usize,
}

/// A validated, zero-origin portable package request. Native subtitles are a
/// presentation rendition and deliberately do not alter the video recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineSpec {
    pub target_height: i64,
    pub audio_index: Option<i64>,
    pub subtitle: OfflineSubtitle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineSubtitle {
    None,
    Native(i64),
    Burn(i64),
}

/// Production outcomes that a durable queue can handle without inventing an
/// encoder failure for ordinary yielding or coalescing.
#[derive(Debug, Clone)]
pub enum OfflineProduceOutcome {
    Ready(Produced),
    Cached(Produced),
    Yielded,
    ClaimedElsewhere,
}

/// The policy and source shared by the cache-claim and encoder stages of one
/// portable production attempt. Keeping these together makes it much harder
/// for a resume path to accidentally change one input between the two stages.
#[derive(Clone, Copy)]
struct PortableProduction<'a> {
    file: &'a plurx_core::domain::MediaFile,
    opts: &'a TranscodeOptions,
    encoder: Encoder,
    deadline: Instant,
    yield_to_offline: bool,
    cancelled: Option<&'a tokio_util::sync::CancellationToken>,
    offline_package_id: Option<&'a str>,
}

/// Everything an earlier pass already encoded, in order.
///
/// Contiguity is what makes this a simple walk: a part that produced nothing is
/// deleted rather than left as a gap, so the first missing number is the end.
/// Reading the parts back off disk — rather than recording a resume point in
/// the database — keeps the bookmark and the bytes the same fact, so they
/// cannot disagree after a crash between writing one and the other.
async fn resume_parts(temp: &std::path::Path) -> Vec<crate::produce::Part> {
    let mut parts = Vec::new();
    loop {
        let dir = temp.join(crate::produce::part_dir(parts.len()));
        if tokio::fs::metadata(&dir).await.is_err() {
            return parts;
        }
        let part = read_part(&dir).await;
        if part.is_empty() {
            // A directory with no listed segments contributes nothing and
            // would shift every later part's numbering if it were counted.
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return parts;
        }
        parts.push(part);
    }
}

/// Read what one part actually produced, from the playlist ffmpeg wrote.
///
/// The playlist rather than a directory listing, because a directory contains
/// the segment that was being written when the process was killed and the
/// playlist does not — an unlisted `.ts` file is a truncated one, and treating
/// it as content puts a corrupt two seconds into the middle of a film.
async fn read_part(part_dir: &std::path::Path) -> crate::produce::Part {
    match tokio::fs::read_to_string(part_dir.join("index.m3u8")).await {
        Ok(text) => crate::produce::Part::from_playlist(&text),
        Err(_) => crate::produce::Part {
            segments: Vec::new(),
            durations_ms: Vec::new(),
        },
    }
}

/// Move every part's segments into one flat directory, write the VOD playlist,
/// and clear the part directories away.
///
/// Renames rather than copies: everything is inside one temp directory on one
/// filesystem, so this costs nothing however large the asset.
async fn publish_from(
    temp: &std::path::Path,
    parts: &[crate::produce::Part],
) -> Result<Option<Published>, String> {
    let assembled = crate::produce::assemble(parts);
    if assembled.placements.is_empty() {
        return Ok(None);
    }
    let mut bytes = 0i64;
    for p in &assembled.placements {
        let from = temp.join(&p.from);
        let to = temp.join(&p.to);
        bytes += tokio::fs::metadata(&from)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| format!("placing {}: {e}", p.to))?;
    }
    tokio::fs::write(temp.join("index.m3u8"), assembled.playlist.as_bytes())
        .await
        .map_err(|e| format!("writing the playlist: {e}"))?;
    bytes += assembled.playlist.len() as i64;
    // The part directories are empty now; what is left in them is ffmpeg's own
    // playlist and any segment it never listed.
    for i in 0..parts.len() {
        let _ = tokio::fs::remove_dir_all(temp.join(crate::produce::part_dir(i))).await;
    }
    Ok(Some(Published {
        bytes,
        duration_ms: assembled.duration_ms,
        segments: assembled.placements.len(),
        parts: parts.len(),
    }))
}

/// Where this node keeps finished transcodes, and what identifies its output.
#[derive(Debug, Clone)]
struct CacheConfig {
    /// Root the location rows are relative to. Deliberately *not* under the
    /// session scratch dir, which is wiped at every boot: a cache that empties
    /// on restart is a warm-up cost with none of the benefit.
    dir: PathBuf,
    ffmpeg_build: String,
    node_id: String,
}

/// One atomically published answer to the operator's requested rate control.
///
/// `quality_rc` is for the exact requested override (or each family's
/// candidate default), not a version-derived guess. A session reads this once
/// while its normalized options are built and keeps that effective value for
/// its lifetime.
#[derive(Debug, Clone, Copy)]
struct RateControlSnapshot {
    requested_mode: RateMode,
    requested_quality: Option<u8>,
    quality_rc: QualityRc,
}

const RATE_CONTROL_REFRESH: Duration = Duration::from_secs(2);

/// Normalize the complete durable request pair. Missing/empty quality is the
/// valid "use this family's default" value; a present nonempty value that does
/// not fit `u8` is corruption and fails the entire pair back to legacy VBR.
pub(crate) fn normalize_rate_control_request(
    raw_mode: Option<&str>,
    raw_quality: Option<&str>,
) -> (RateMode, Option<u8>, bool) {
    let mode_text = raw_mode.map(str::trim).filter(|value| !value.is_empty());
    let mode = mode_text.and_then(RateMode::parse);
    let quality_text = raw_quality.map(str::trim).filter(|value| !value.is_empty());
    let quality = quality_text.and_then(|value| value.parse::<u8>().ok());
    let corrupt =
        mode_text.is_some() && mode.is_none() || quality_text.is_some() && quality.is_none();
    if corrupt {
        (RateMode::Bitrate, None, true)
    } else {
        (mode.unwrap_or_default(), quality, false)
    }
}

enum RateControlValidation {
    Complete(RateControlSnapshot),
    Deferred,
}

#[derive(Clone, Copy)]
enum RateControlProbePolicy {
    Boot,
    YieldingBackground,
}

#[derive(Debug)]
pub enum ApplyRateControlError {
    Store(plurx_core::error::StoreError),
    Busy,
}

impl From<plurx_core::error::StoreError> for ApplyRateControlError {
    fn from(error: plurx_core::error::StoreError) -> Self {
        Self::Store(error)
    }
}

impl RateControlSnapshot {
    fn bitrate(boot_caps: QualityRc) -> Self {
        Self {
            requested_mode: RateMode::Bitrate,
            requested_quality: None,
            quality_rc: boot_caps,
        }
    }

    fn effective_for(self, encoder: Encoder) -> EffectiveRateControl {
        if self.requested_mode == RateMode::Quality && self.quality_rc.supported_by(encoder) {
            EffectiveRateControl::Qvbr {
                quality: self
                    .requested_quality
                    .unwrap_or_else(|| encoder.default_quality()),
            }
        } else {
            EffectiveRateControl::Vbr
        }
    }
}

pub struct TranscodeManager {
    store: Arc<dyn Store>,
    work_dir: PathBuf,
    /// Writable XDG cache inherited by ffmpeg and libraries such as fontconfig.
    runtime_cache: PathBuf,
    /// Extracted text subtitles shared with the WebVTT endpoint.
    subtitle_cache: PathBuf,
    caps: EncoderCaps,
    /// Validated hot rate-control state. Published only after every usable
    /// family has completed its production-argument probe.
    rate_control: std::sync::RwLock<RateControlSnapshot>,
    /// Serializes probe → durable settings → publication. Without this, two
    /// concurrent admin PUTs can leave the store describing one request and
    /// the in-memory effective snapshot describing the other.
    rate_control_update: Mutex<()>,
    /// The tone-map graph this node proved it can run, or [`Pipeline::Cpu`]
    /// when nothing did. A per-session decision still filters it — see
    /// [`Pipeline::for_session`] — because a proven graph is a claim about the
    /// box, not about the session.
    pipeline: Pipeline,
    /// The hardware budget, and what this box has learned about its own speed.
    admissions: Admissions,
    /// Where finished transcodes live, and what identifies them here. `None`
    /// until configured, which is a real state rather than a placeholder: a
    /// node with no cache root simply always misses, and every path below is
    /// written so that a miss is the ordinary case.
    cache: Option<CacheConfig>,
    /// Shared with cache housekeeping. A row can say bytes exist, but only
    /// this registry can say an HTTP session on this node is using them now.
    cache_readers: crate::cachekeep::ActiveCacheReaders,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Creation requests by `request_id` — reserved *before* work starts, so
    /// two concurrent creates with the same id cannot both pass the check and
    /// spawn two encoders (the check-then-act race this map used to have).
    /// Values are `(fingerprint, state)`. Small by construction — a `Ready`
    /// entry is dropped as soon as its session is gone, an `InFlight` one the
    /// moment its create resolves or is abandoned, and one player instance
    /// has one in flight at a time.
    ///
    /// A `std` mutex rather than tokio's, never held across an await: the
    /// abandoned-create cleanup runs in a `Drop` impl, and `Drop` cannot
    /// await.
    requests: std::sync::Mutex<HashMap<String, (String, RequestState)>>,
    /// See [`ProducerTuning`]. Always the default outside tests.
    producer: ProducerTuning,
    /// Scheduled and user-requested cache producers share one writer. A queued
    /// offline request asks speculative work to stop at its next published
    /// segment boundary, then takes this gate before resuming its own claim.
    background_producer: Mutex<()>,
    offline_waiting: AtomicBool,
    /// Whether this daemon's ffmpeg can strip a Dolby Vision configuration —
    /// probed at boot ([`crate::ffmpeg::has_dovi_rpu`]).
    ///
    /// Its own field because the copy path used to derive this from the
    /// *cache config*, which happens to carry a copy of the ffmpeg version
    /// string: a node with no cache configured would answer "no dovi_rpu"
    /// whatever ffmpeg it was running, leave the DV configuration in every
    /// remux, and have browsers that cannot decode DV refuse the stream.
    dv_strippable: bool,
    /// The ahead-window limits, snapshotted ([`AHEAD_LIMITS_TTL`]).
    ///
    /// Flow control consults the limits on every segment publish and
    /// frontier advance — which used to mean three SQLite round-trips a
    /// time, through the store's one serialized connection, on the hottest
    /// control path the server has (review §2.6). The keys are API-settable
    /// with no UI knob, so a two-second staleness bound is invisible to an
    /// operator and removes the reads from the path entirely.
    cached_limits: std::sync::RwLock<Option<(Instant, AheadLimits)>>,
}

impl TranscodeManager {
    /// `pipeline` is the tone-map graph this node proved at boot — see
    /// [`crate::pipeprobe`]. It is fixed for the manager's life because it is
    /// a fact about the hardware, not a setting; the per-session filtering
    /// that decides whether a given stream may actually use it lives in
    /// [`Pipeline::for_session`].
    pub fn new(
        store: Arc<dyn Store>,
        work_dir: PathBuf,
        caps: EncoderCaps,
        pipeline: Pipeline,
    ) -> Self {
        // Managers without a finished-transcode cache (mainly tests and
        // embedded uses) still get a writable location without reaching
        // outside their scratch root. `with_cache` moves this beside the
        // persistent caches in a normal daemon.
        let runtime_cache = work_dir.join(".runtime-cache");
        let subtitle_cache = work_dir.join(".subtitle-cache");
        if let Err(err) = std::fs::create_dir_all(&runtime_cache) {
            tracing::warn!(
                path = %runtime_cache.display(),
                "could not create ffmpeg runtime cache: {err}"
            );
        }
        TranscodeManager {
            store,
            work_dir,
            runtime_cache,
            subtitle_cache,
            rate_control: std::sync::RwLock::new(RateControlSnapshot::bitrate(caps.quality_rc)),
            rate_control_update: Mutex::new(()),
            caps,
            pipeline,
            admissions: Admissions::new(),
            cache: None,
            cache_readers: crate::cachekeep::ActiveCacheReaders::default(),
            sessions: Mutex::new(HashMap::new()),
            requests: std::sync::Mutex::new(HashMap::new()),
            producer: ProducerTuning::default(),
            background_producer: Mutex::new(()),
            offline_waiting: AtomicBool::new(false),
            dv_strippable: false,
            cached_limits: std::sync::RwLock::new(None),
        }
    }

    /// Point the manager at a cache root, and tell it what this node's output
    /// is identified by.
    ///
    /// Separate from the constructor because both pieces come from elsewhere:
    /// the ffmpeg build from the startup probe, the node id from the store.
    /// Chaining keeps every existing call site — and every test that does not
    /// care about caching — unchanged.
    /// Record what the boot probe found out about this daemon's ffmpeg.
    /// Independent of the cache: the capabilities it gates are the daemon's,
    /// not the cache's.
    pub fn with_dv_strippable(mut self, dv_strippable: bool) -> Self {
        self.dv_strippable = dv_strippable;
        self
    }

    /// Whether this build can strip a Dolby Vision configuration
    /// (`dovi_rpu`, ffmpeg 7.1+).
    pub fn dv_strippable(&self) -> bool {
        self.dv_strippable
    }

    pub fn with_cache(mut self, cache_dir: PathBuf, ffmpeg_build: String, node_id: String) -> Self {
        let cache_parent = cache_dir.parent().unwrap_or(cache_dir.as_path());
        self.runtime_cache = cache_parent.join("runtime");
        self.subtitle_cache = cache_parent.join("subs");
        if let Err(err) = std::fs::create_dir_all(&self.runtime_cache) {
            tracing::warn!(
                path = %self.runtime_cache.display(),
                "could not create ffmpeg runtime cache: {err}"
            );
        }
        self.cache = Some(CacheConfig {
            dir: cache_dir,
            ffmpeg_build,
            node_id,
        });
        self
    }

    pub fn cache_readers(&self) -> &crate::cachekeep::ActiveCacheReaders {
        &self.cache_readers
    }

    #[cfg(test)]
    pub fn begin_cache_eviction_for_test(&self, recipe: &str) -> impl Drop {
        self.cache_readers
            .begin_eviction(recipe)
            .expect("test eviction claim")
    }

    pub fn active_cache_entries(&self) -> usize {
        self.cache_readers.active_entries()
    }

    /// Override [`ProducerTuning`]. Tests only — there is deliberately no
    /// production path that reaches this, so the daemon cannot be configured
    /// into pacing a producer or into a shorter retry by accident.
    #[cfg(test)]
    fn with_producer_tuning(mut self, producer: ProducerTuning) -> Self {
        self.producer = producer;
        self
    }

    /// Where finished transcodes live and what this node is called there, or
    /// `None` when no cache root is configured.
    ///
    /// Handed out rather than duplicated in the caller: the housekeeping sweep
    /// and the serving path have to agree about both, and two copies of "the
    /// cache root" is how they come to disagree after somebody makes one
    /// configurable.
    pub fn cache_location(&self) -> Option<(&std::path::Path, &str)> {
        self.cache
            .as_ref()
            .map(|c| (c.dir.as_path(), c.node_id.as_str()))
    }

    pub fn subtitle_cache_dir(&self) -> &std::path::Path {
        &self.subtitle_cache
    }

    /// This node's output identity, for naming a transcode.
    fn digest(&self) -> Option<PipelineDigest> {
        let cache = self.cache.as_ref()?;
        Some(PipelineDigest {
            ffmpeg_build: cache.ffmpeg_build.clone(),
            encoder: Encoder::Software, // replaced per lookup
            pipeline: self.pipeline,
        })
    }

    /// The only constructor for a content-addressed transcode recipe.
    ///
    /// `opts` is already normalized and contains only validated effective rate
    /// control. Setting the encoder here prevents live lookup, speculative
    /// production, and offline production from drifting into different names
    /// for the same output. The manager-level pipeline remains byte-for-byte
    /// where the legacy recipe put it; changing that is a separate cache
    /// contract, not part of N1.
    fn effective_recipe<'a>(
        &self,
        digest: &'a mut PipelineDigest,
        file: &'a plurx_core::domain::MediaFile,
        opts: &'a TranscodeOptions,
        encoder: Encoder,
        audio_copied: bool,
    ) -> Recipe<'a> {
        digest.encoder = encoder;
        Recipe {
            digest,
            file,
            opts,
            audio_copied,
        }
    }

    /// Which audio track a session carries, and which subtitle it burns.
    ///
    /// One function, called by the live path and by the producer, because the
    /// answer is part of the recipe: two sessions differing only in audio track
    /// are different bytes. A producer that skipped this and a playback that
    /// did it would compute two different names for the same film, and the
    /// symptom is a cache that fills forever and never hits — which looks
    /// exactly like a cache that is merely cold. It was, in fact, the first
    /// thing that went wrong when the producer was wired up.
    async fn select_tracks(
        &self,
        file: &plurx_core::domain::MediaFile,
        audio_override: Option<i64>,
        subtitle_override: Option<i64>,
    ) -> Tracks {
        // Prefer original (Japanese) audio + subs when the file is dual-audio
        // anime-style (REQ-SUB-2), and honour the server-wide language
        // preferences otherwise. Native WebVTT-capable tracks remain media
        // renditions; only bitmap/styled fallbacks are drawn into the video.
        let prefer_original = file
            .audio_streams
            .iter()
            .any(|a| matches!(a.language.as_deref(), Some("jpn" | "ja" | "jp")))
            && file.audio_streams.len() > 1;
        let prefs = self.lang_prefs().await;
        let selection = plurx_core::tracks::select_tracks(
            &file.audio_streams,
            &file.subtitle_streams,
            prefer_original,
            &prefs,
        );
        // A viewer's explicit choice wins over the automatic one, and is the
        // only way a bitmap subtitle is ever burned: the automatic rule exists
        // for dual-audio anime, where burning is a guess at what somebody
        // wants. Burning a subtitle nobody asked for is a picture they cannot
        // turn off.
        let burn_index = subtitle_override.filter(|i| *i >= 0).or_else(|| {
            prefer_original
                .then_some(selection.subtitle_index)
                .flatten()
                .filter(|idx| {
                    file.subtitle_streams
                        .get(*idx as usize)
                        .is_some_and(|s| plurx_core::tracks::subtitle_requires_burn(&s.codec))
                })
        });
        Tracks {
            audio_index: audio_override.or(selection.audio_index),
            subtitle_burn: burn_index.and_then(|idx| {
                let codec = file
                    .subtitle_streams
                    .get(idx as usize)
                    .map(|s| s.codec.clone())?;
                Some(plurx_core::transcode::SubtitleBurn {
                    subtitle_index: idx,
                    bitmap: plurx_core::tracks::is_bitmap_subtitle(&codec),
                })
            }),
        }
    }

    /// The options a session with this shape would run with.
    ///
    /// One builder, because the cache asks the question twice — once to name
    /// what it is looking for, once to name what it is about to produce — and
    /// two spellings of "what this session is" would eventually disagree,
    /// which is a cache that never hits or, worse, one that hits wrongly.
    #[allow(clippy::too_many_arguments)] // one session's worth of knobs
    fn options_for(
        &self,
        encoder: Encoder,
        file: &plurx_core::domain::MediaFile,
        target_height: i64,
        start_seconds: f64,
        audio_index: Option<i64>,
        subtitle_burn: Option<plurx_core::transcode::SubtitleBurn>,
        software_threads: Option<u32>,
    ) -> TranscodeOptions {
        self.options_for_tone_map(
            encoder,
            file,
            target_height,
            start_seconds,
            audio_index,
            subtitle_burn,
            software_threads,
            tone_map_pref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn options_for_tone_map(
        &self,
        encoder: Encoder,
        file: &plurx_core::domain::MediaFile,
        target_height: i64,
        start_seconds: f64,
        audio_index: Option<i64>,
        subtitle_burn: Option<plurx_core::transcode::SubtitleBurn>,
        software_threads: Option<u32>,
        tone_map: ToneMap,
    ) -> TranscodeOptions {
        let subtitle_file = self.subtitle_file(file, subtitle_burn.as_ref());
        TranscodeOptions {
            target_height,
            software_threads,
            video_bitrate_kbps: bitrate_for_height(target_height),
            effective_rate_control: self.effective_rate_control(encoder),
            audio_index,
            start_seconds,
            tone_map,
            // The node proved a graph; this session may still not be entitled
            // to it (HLG, non-compatible Dolby Vision, a light source, an
            // encoder it cannot feed). Deciding once, here,
            // is what keeps the log line honest — `pipeline=` is what actually
            // ran, not what the box is capable of. Routed by `routing_hdr`
            // rather than the raw column: a DV base layer that is
            // HDR10-compatible is an hdr10 stream to a tone-map, and a bitmap
            // subtitle burn keeps the GPU graph (it downloads once for
            // libass/overlay after the expensive scale + tone-map is done).
            pipeline: Pipeline::for_session(
                self.pipeline,
                encoder,
                transcode::routing_hdr(file),
                transcode::heavy_source(file),
                subtitle_burn.as_ref().is_some_and(|b| !b.bitmap),
            ),
            subtitle_burn,
            subtitle_file,
            // Only where the startup probe proved this build takes it. A
            // family that needs the flag and cannot have it still works; its
            // segments just follow the encoder's GOP, which is slower to start
            // and is logged as such at boot.
            force_idr: self.caps.forced_idr.wanted_by(encoder),
            ..Default::default()
        }
    }

    /// Keep resumable offline packages on the immutable pre-N1 identity until
    /// their effective rate control can be persisted with the package row.
    ///
    /// A package may yield and resume after the global setting changes. Using
    /// that mutable setting would rebuild a different recipe and strand the
    /// package behind its already-pinned hash. The durable snapshot needs a
    /// ratified storage migration, so the safe interim behavior is legacy VBR.
    fn pin_offline_rate_control(opts: &mut TranscodeOptions) {
        opts.effective_rate_control = EffectiveRateControl::Vbr;
    }

    /// A lossless-enough sidecar for simple text codecs. ASS/SSA remains
    /// embedded because converting it to WebVTT would discard positioning,
    /// styles, and the release's authored typography.
    fn subtitle_file(
        &self,
        file: &plurx_core::domain::MediaFile,
        burn: Option<&plurx_core::transcode::SubtitleBurn>,
    ) -> Option<PathBuf> {
        let burn = burn.filter(|burn| !burn.bitmap)?;
        let codec = file
            .subtitle_streams
            .get(burn.subtitle_index as usize)?
            .codec
            .to_lowercase();
        matches!(codec.as_str(), "subrip" | "srt" | "webvtt" | "mov_text")
            .then(|| crate::subtitles::vtt_path(&self.subtitle_cache, file, burn.subtitle_index))
    }

    /// Materialise a text subtitle before ffmpeg opens the video pipeline.
    /// libass reopening the MKV itself reads the entire movie before frame one;
    /// the small cached VTT opens immediately and is shared with `/subs`.
    async fn ensure_text_subtitle(
        &self,
        file: &plurx_core::domain::MediaFile,
        burn: Option<&plurx_core::transcode::SubtitleBurn>,
    ) -> Result<(), String> {
        let Some(burn) = burn else {
            return Ok(());
        };
        if self.subtitle_file(file, Some(burn)).is_none() {
            return Ok(());
        }
        crate::subtitles::ensure_vtt(&self.subtitle_cache, file, burn.subtitle_index)
            .await
            .map(|_| ())
    }

    /// Serve a finished transcode, if this exact one has already been made.
    ///
    /// Registers a session with no child process. That is the whole shape of a
    /// hit: there is nothing to run, nothing to watch, nothing to pace, and
    /// nothing to suspend — only a directory of segments that already exist
    /// and a viewer to point at them. It is still a session because the
    /// activity page should show somebody watching, and because the idle
    /// reaper is what eventually forgets them.
    async fn serve_cached(
        &self,
        file: &plurx_core::domain::MediaFile,
        opts: &TranscodeOptions,
        encoder: Encoder,
        item_title: &str,
        user_name: &str,
        playback_id: &str,
    ) -> Option<StartInfo> {
        let cache = self.cache.as_ref()?;
        let mut digest = self.digest()?;
        let hash = self
            .effective_recipe(&mut digest, file, opts, encoder, false)
            .hash();

        // Claim before looking at either the row or the filesystem. An
        // eviction already in progress turns this into an ordinary miss; a
        // successful lookup carries the guard in the Session until every
        // response using that session is gone.
        let Some(cache_reader) = self.cache_readers.begin_read(&hash) else {
            tracing::debug!(recipe = %hash, file = file.id, "cache entry is being evicted");
            return None;
        };

        let hit = match self.store.cache_hit(&hash, &cache.node_id).await {
            Ok(Some(hit)) => hit,
            other => {
                // The name is logged on a miss because "why is this not
                // hitting?" is otherwise unanswerable from outside: the hash
                // is a pure function of a dozen inputs, and a producer and a
                // player disagreeing about any one of them looks identical to
                // an empty cache. With the name in both logs the disagreement
                // is one `grep` rather than a bisect.
                if let Err(e) = other {
                    tracing::warn!(recipe = %hash, error = %e, "cache lookup failed");
                }
                tracing::debug!(recipe = %hash, file = file.id, "transcode cache miss");
                return None;
            }
        };
        let dir = cache.dir.join(&hit.relative_dir);
        // The row says the bytes are there; the disk is what actually has to
        // have them. A cache root on a mount that did not come back after a
        // reboot would otherwise serve a playlist for an empty directory —
        // the row survives what the filesystem does not.
        if tokio::fs::metadata(dir.join("index.m3u8")).await.is_err() {
            tracing::warn!(
                recipe = %hash, dir = %dir.display(),
                "cache row points at a directory with no playlist — treating as a miss"
            );
            return None;
        }
        let _ = self.store.touch_cache_entry(&hash, &cache.node_id).await;

        let session_id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(Session {
            dir,
            child: Mutex::new(None),
            cached: true,
            _cache_reader: Some(cache_reader),
            last_request: Mutex::new(LastRequest::now("session-start")),
            file_id: file.id,
            item_id: file.item_id,
            item_title: item_title.to_owned(),
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            // A cache hit only ever answers a transcode request (`serve_cached`
            // is reached from the transcode path alone); the encoder label goes to
            // "cached" here, which is exactly why the method is not read off it.
            method: crate::delivery::Method::Transcode,
            start_seconds: 0.0,
            // A cache hit is the whole stream from the beginning.
            media_origin_seconds: 0.0,
            hls_codecs: "avc1.640034,mp4a.40.2".into(),
            hls_supplemental_codecs: None,
            target_height: opts.target_height,
            encoder_label: Mutex::new("cached"),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            playlist_published: AtomicBool::new(true),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            live_bytes: AtomicI64::new(0),
            progress: Arc::new(Progress::new()),
            class: std::sync::Mutex::new(String::new()),
            hw_slot: std::sync::Mutex::new(None),
            sw_permit: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            readrate: 0.0,
            suspended: AtomicBool::new(false),
            suspended_at: Mutex::new(None),
            suspend_count: AtomicU64::new(0),
            typeless_sliding: false,
            first_slide_logged: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));
        tracing::info!(
            %session_id, recipe = %hash, file = file.id,
            "serving a cached transcode — no encoder started"
        );
        self.emit_session_event(
            &session_id,
            &session,
            "session_start",
            SessionEventFields {
                extra: Some(serde_json::json!({ "cache": "hit" }).to_string()),
                ..SessionEventFields::default()
            },
        )
        .await;
        Some(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id,
            duration_ms: file.duration_ms,
            // A cached asset is the whole title, so playback starts at zero and
            // the player seeks into it. `start_seconds` on a live session
            // exists because the encoder had to be told where to begin; here
            // there is no encoder and nothing to tell.
            start_seconds: 0.0,
            media_origin_seconds: 0.0,
            encoder: "cached",
            vod: true,
        })
    }

    // ---- the pre-transcode producer (PERF-PLAN §6.2) -----------------------

    /// Pre-transcode one file at one rung, so the next viewer gets a cache hit.
    ///
    /// Runs at background priority and is expected to be interrupted: an encode
    /// of a two-hour 4K film will normally be preempted several times by people
    /// pressing play, and picks up from its last published segment boundary
    /// each time — within this call, and across later ones. Preemption is by
    /// *termination* — see [`crate::admission`] for why suspending would not
    /// release anything that matters.
    ///
    /// **One of these at a time per node.** Resuming rests on it: an incomplete
    /// claim held by this node is read as "an earlier pass of mine stopped
    /// here", and two concurrent producers would each read the other's live
    /// work that way and encode into the same staging directory.
    /// [`crate::state::JobManager::produce_pass`] is the only caller and holds
    /// a flag that enforces it.
    ///
    /// Returns the recipe hash on success, `None` when there was nothing to do
    /// (already cached, already claimed by another producer, no cache
    /// configured) — neither of which is a failure.
    pub async fn produce(
        &self,
        file: &plurx_core::domain::MediaFile,
        target_height: i64,
        deadline: Instant,
    ) -> Result<Option<Produced>, String> {
        if self.cache.is_none() {
            return Ok(None);
        }
        if self
            .offline_waiting
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(None);
        }
        let _producer = match self.background_producer.try_lock() {
            Ok(permit) => permit,
            Err(_) => return Ok(None),
        };
        let rate_control = self.rate_control_snapshot();
        // A background artifact is the zero-offset default shared by future
        // plays. Never bake a historical, file-persisted correction into it.
        let mut playback_file = file.clone();
        playback_file.audio_offset_ms = 0;
        let file = &playback_file;
        let encoder = self.encoder().await;
        // Through the same track selection a real playback uses. Not an
        // optimisation — the tracks are part of the recipe, so producing with
        // "no audio track chosen" makes an entry named for a session that will
        // never be requested.
        let Tracks {
            audio_index,
            subtitle_burn,
        } = self.select_tracks(file, None, None).await;
        let mut opts = self.options_for(
            encoder,
            file,
            target_height,
            0.0,
            audio_index,
            subtitle_burn,
            // Parts pick their own budget from the Background permit they run
            // under (produce_into) — and the recipe hash excludes it anyway.
            None,
        );
        opts.effective_rate_control = rate_control.effective_for(encoder);
        let mut digest = self.digest().ok_or("no cache digest")?;
        let hash = self
            .effective_recipe(&mut digest, file, &opts, encoder, false)
            .hash();

        Ok(
            match self
                .produce_normalized(
                    PortableProduction {
                        file,
                        opts: &opts,
                        encoder,
                        deadline,
                        yield_to_offline: true,
                        cancelled: None,
                        offline_package_id: None,
                    },
                    hash,
                )
                .await?
            {
                OfflineProduceOutcome::Ready(produced) => Some(produced),
                OfflineProduceOutcome::Cached(_)
                | OfflineProduceOutcome::Yielded
                | OfflineProduceOutcome::ClaimedElsewhere => None,
            },
        )
    }

    /// Prepare the exact mobile package requested by an authenticated user.
    /// Unlike speculative production, this preserves the file's A/V offset,
    /// accepts explicit tracks, and forces SDR even on a passthrough node.
    pub async fn ensure_offline(
        &self,
        package_id: &str,
        file: &plurx_core::domain::MediaFile,
        spec: &OfflineSpec,
        deadline: Instant,
        cancelled: &tokio_util::sync::CancellationToken,
    ) -> Result<OfflineProduceOutcome, String> {
        if cancelled.is_cancelled() {
            return Ok(OfflineProduceOutcome::Yielded);
        }
        struct Waiting<'a>(&'a AtomicBool);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        self.offline_waiting
            .store(true, std::sync::atomic::Ordering::Release);
        let waiting = Waiting(&self.offline_waiting);
        let _producer = self.background_producer.lock().await;
        drop(waiting);
        if cancelled.is_cancelled() {
            return Ok(OfflineProduceOutcome::Yielded);
        }
        let encoder = self.encoder().await;
        let subtitle_burn = match spec.subtitle {
            OfflineSubtitle::Burn(index) => {
                let stream = file
                    .subtitle_streams
                    .iter()
                    .find(|stream| stream.index == index)
                    .ok_or_else(|| "offline subtitle track disappeared".to_owned())?;
                Some(plurx_core::transcode::SubtitleBurn {
                    subtitle_index: index,
                    bitmap: plurx_core::tracks::is_bitmap_subtitle(&stream.codec),
                })
            }
            OfflineSubtitle::None | OfflineSubtitle::Native(_) => None,
        };
        let mut opts = self.options_for_tone_map(
            encoder,
            file,
            spec.target_height,
            0.0,
            spec.audio_index,
            subtitle_burn,
            None,
            ToneMap::Zscale,
        );
        Self::pin_offline_rate_control(&mut opts);
        let mut digest = self.digest().ok_or("no cache digest")?;
        let hash = self
            .effective_recipe(&mut digest, file, &opts, encoder, false)
            .hash();
        if !self
            .store
            .set_offline_package_recipe(package_id, &hash)
            .await
            .map_err(|error| error.to_string())?
        {
            return if cancelled.is_cancelled() {
                Ok(OfflineProduceOutcome::Yielded)
            } else {
                Err("offline package is no longer preparing".to_owned())
            };
        }
        let outcome = self
            .produce_normalized(
                PortableProduction {
                    file,
                    opts: &opts,
                    encoder,
                    deadline,
                    yield_to_offline: false,
                    cancelled: Some(cancelled),
                    offline_package_id: Some(package_id),
                },
                hash,
            )
            .await?;
        if matches!(
            outcome,
            OfflineProduceOutcome::Ready(_) | OfflineProduceOutcome::Cached(_)
        ) {
            if let OfflineSubtitle::Native(index) = spec.subtitle {
                let _ = self
                    .store
                    .update_offline_progress(package_id, "extracting_subtitles", 999)
                    .await;
                crate::subtitles::ensure_vtt(&self.subtitle_cache, file, index).await?;
            }
        }
        Ok(outcome)
    }

    /// Shared content-addressed production tail. Track and eligibility policy
    /// live above this point; claiming, resume, publication, and cache identity
    /// live here once for scheduled and requested work.
    async fn produce_normalized(
        &self,
        request: PortableProduction<'_>,
        hash: String,
    ) -> Result<OfflineProduceOutcome, String> {
        let PortableProduction {
            file,
            opts,
            encoder: _,
            deadline: _,
            yield_to_offline: _,
            cancelled: _,
            offline_package_id,
        } = request;
        let cache = self.cache.as_ref().ok_or("no cache configured")?;
        if let Some(cached) = self
            .store
            .cache_hit(&hash, &cache.node_id)
            .await
            .map_err(|error| error.to_string())?
        {
            let playlist =
                tokio::fs::read_to_string(cache.dir.join(&cached.relative_dir).join("index.m3u8"))
                    .await
                    .map_err(|error| format!("reading cached offline playlist: {error}"))?;
            if !playlist.contains("#EXT-X-ENDLIST") {
                return Err("complete cache row contains a non-VOD playlist".to_owned());
            }
            let part = crate::produce::Part::from_playlist(&playlist);
            if let Some(package_id) = offline_package_id {
                let _ = self
                    .store
                    .update_offline_progress(package_id, "transcoding", 999)
                    .await;
            }
            return Ok(OfflineProduceOutcome::Cached(Produced {
                recipe: hash,
                bytes: cached.bytes,
                duration_ms: part.duration_ms(),
                segments: part.segments.len(),
                parts: 0,
            }));
        }

        self.ensure_text_subtitle(file, opts.subtitle_burn.as_ref())
            .await?;
        let relative = format!("{}/{hash}", &hash[..2]);
        let taken = self
            .store
            .claim_cache_entry(
                &hash,
                file.id,
                CACHE_RECIPE_VERSION,
                &cache.node_id,
                &relative,
            )
            .await
            .map_err(|error| error.to_string())?;
        let temp = crate::cachekeep::staging_dir(&cache.dir, &hash);
        if !taken {
            if tokio::fs::metadata(&temp).await.is_err() {
                tracing::debug!(
                    recipe = %hash,
                    file = file.id,
                    "cache entry claimed elsewhere; standing down"
                );
                return Ok(OfflineProduceOutcome::ClaimedElsewhere);
            }
            tracing::info!(
                recipe = %hash,
                file = file.id,
                "resuming a portable transcode left unfinished"
            );
        }

        let published = match self.produce_into(&temp, &hash, &request).await {
            Ok(Some(published)) => published,
            Ok(None) => {
                self.touch_claim(&hash, &cache.node_id).await;
                return Ok(OfflineProduceOutcome::Yielded);
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&temp).await;
                let _ = self
                    .store
                    .forget_cache_entry(&hash, &cache.node_id, "local")
                    .await;
                return Err(error);
            }
        };

        let final_dir = cache.dir.join(&relative);
        if let Some(parent) = final_dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        tokio::fs::rename(&temp, &final_dir)
            .await
            .map_err(|error| format!("publishing {}: {error}", final_dir.display()))?;
        self.store
            .complete_cache_entry(&hash, &cache.node_id, published.bytes)
            .await
            .map_err(|error| error.to_string())?;
        tracing::info!(
            recipe = %hash,
            file = file.id,
            height = opts.target_height,
            bytes = published.bytes,
            duration_s = published.duration_ms / 1000,
            segments = published.segments,
            parts = published.parts,
            "portable transcode published"
        );
        Ok(OfflineProduceOutcome::Ready(Produced {
            recipe: hash,
            bytes: published.bytes,
            duration_ms: published.duration_ms,
            segments: published.segments,
            parts: published.parts,
        }))
    }

    /// Keep a resumable claim from ageing into a crash leftover.
    ///
    /// Without this a film that needs several passes would have its bookmark
    /// swept a day after the first one, and every pass after that would start
    /// from zero — the loop that never finishes, wearing the disguise of a
    /// cleanup working correctly.
    async fn touch_claim(&self, hash: &str, node_id: &str) {
        if let Err(e) = self.store.touch_cache_claim(hash, node_id).await {
            tracing::warn!(recipe = %hash, error = %e, "could not mark a pre-transcode as still in progress");
        }
    }

    /// Encode into `temp` until finished, out of time, or out of patience with
    /// being preempted. `Ok(None)` means nothing publishable was produced.
    async fn produce_into(
        &self,
        temp: &std::path::Path,
        hash: &str,
        request: &PortableProduction<'_>,
    ) -> Result<Option<Published>, String> {
        let PortableProduction {
            file,
            opts,
            encoder,
            deadline,
            yield_to_offline,
            cancelled,
            offline_package_id,
        } = *request;
        tokio::fs::create_dir_all(temp)
            .await
            .map_err(|e| format!("creating {}: {e}", temp.display()))?;
        let max = self.max_hw_sessions().await;
        // Whatever an earlier pass got through. Usually nothing; on a busy box
        // making a long film, this is how it eventually finishes.
        let mut parts = resume_parts(temp).await;
        if !parts.is_empty() {
            tracing::info!(
                recipe = %hash, parts = parts.len(),
                from_s = crate::produce::resume_at_ms(&parts) / 1000,
                "picking up where an earlier pass stopped"
            );
        }
        // Counted separately from `parts` because a part that was killed
        // before its first segment produced nothing and so is not one — but it
        // did start an encoder, which is the thing worth bounding.
        let mut spawned = 0usize;

        while spawned < PRODUCER_MAX_PARTS {
            if cancelled.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                return Ok(None);
            }
            if yield_to_offline
                && self
                    .offline_waiting
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                tracing::debug!(recipe = %hash, "producer out of time for this run");
                return Ok(None);
            }
            // Do not even start while a viewer is queuing — and never spend
            // what a viewer would want.
            //
            // For a hardware encoder the slot request answers both, since
            // background acquisition is refused outright while anyone is in
            // the queue. Software parts draw from the CPU pool at Background
            // priority now (§2.4): the same refusal while a viewer queues —
            // which also keeps this loop from spawning ffmpeg just to kill it
            // on the first poll, hundreds of times a second — plus a real
            // reservation, so producer parts and live software sessions can
            // no longer oversubscribe every core between them.
            let mut sw_hold = None;
            let slot = if encoder == Encoder::Software {
                match self.admissions.try_admit_software(
                    self.software_budget().await,
                    Workload::of(file, opts.target_height).software_threads(),
                    Priority::Background,
                ) {
                    Some(permit) => {
                        sw_hold = Some(permit);
                        None
                    }
                    None => {
                        tokio::time::sleep(self.producer.retry).await;
                        continue;
                    }
                }
            } else {
                match self.admissions.try_acquire(max, Priority::Background) {
                    Some(slot) => Some(slot),
                    None => {
                        tokio::time::sleep(self.producer.retry).await;
                        continue;
                    }
                }
            };
            spawned += 1;

            let part_dir = temp.join(crate::produce::part_dir(parts.len()));
            tokio::fs::create_dir_all(&part_dir)
                .await
                .map_err(|e| format!("creating {}: {e}", part_dir.display()))?;
            let resume_ms = crate::produce::resume_at_ms(&parts);
            let part_opts = TranscodeOptions {
                start_seconds: resume_ms as f64 / 1000.0,
                // What the Background permit reserved is what this part may
                // spend. Not part of the recipe hash, so resumed parts and
                // cache identity are unaffected.
                software_threads: sw_hold.as_ref().map(|p| p.threads() as u32),
                ..opts.clone()
            };
            // Unpaced, deliberately. Pacing exists so a live session does not
            // write a film ahead of a playhead that will never reach it; a
            // producer has no playhead and every second it spends holding the
            // hardware is a second a viewer might want it. The value is
            // [`ProducerTuning::pacing`], which is `unpaced()` everywhere
            // except the one test that has to interrupt this encoder.
            let args = transcode::hls_args(
                file,
                encoder,
                &part_opts,
                self.producer.pacing,
                &part_dir.to_string_lossy(),
            );
            tracing::info!(
                recipe = %hash, part = parts.len(), from_s = part_opts.start_seconds,
                encoder = encoder.label(), "pre-transcode part starting"
            );
            let progress = Arc::new(Progress::new());
            let generation = progress.begin_attempt();
            let mut child = spawn_ffmpeg(
                &args,
                encoder.label(),
                hash,
                Arc::clone(&progress),
                generation,
                &self.runtime_cache,
            )?;

            let ended = self
                .run_part(&mut child, deadline, yield_to_offline, cancelled)
                .await;
            drop(slot); // before anything else: a viewer is probably waiting on it
            drop(sw_hold); // and the pool share with it
            let part = read_part(&part_dir).await;
            let produced = !part.is_empty();
            if produced {
                parts.push(part);
                if let (Some(package_id), Some(duration_ms)) = (
                    offline_package_id,
                    file.duration_ms.filter(|duration| *duration > 0),
                ) {
                    let completed_ms = crate::produce::resume_at_ms(&parts);
                    let progress = completed_ms
                        .saturating_mul(1000)
                        .saturating_div(duration_ms)
                        .clamp(1, 999);
                    let _ = self
                        .store
                        .update_offline_progress(package_id, "transcoding", progress)
                        .await;
                }
            }

            match ended {
                PartEnd::Finished => return publish_from(temp, &parts).await,
                PartEnd::Preempted | PartEnd::Deadline => {
                    tracing::info!(
                        recipe = %hash, spawned,
                        produced_s = crate::produce::resume_at_ms(&parts) / 1000,
                        "pre-transcode yielded"
                    );
                    // A part that produced nothing leaves an empty directory
                    // that the next part must not reuse a number with.
                    if !produced {
                        let _ = tokio::fs::remove_dir_all(&part_dir).await;
                    }
                    if yield_to_offline
                        && self
                            .offline_waiting
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        return Ok(None);
                    }
                    if cancelled.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                        return Ok(None);
                    }
                    if matches!(ended, PartEnd::Deadline) {
                        // Out of budget for this pass. Nothing is published —
                        // a partial asset must never be serveable — but
                        // nothing is discarded either: the numbered parts
                        // stay on disk under the claim, and the next pass
                        // over this recipe starts at `resume_parts`, picking
                        // up exactly where this one stopped (the log line at
                        // the top of this loop is that event). A film that
                        // cannot finish inside one window therefore still
                        // gets cached — across as many passes as it takes —
                        // provided the claim row survives between them; the
                        // parts and the claim are the checkpoint, and there
                        // is deliberately no second bookmark in the database
                        // to disagree with them after a crash.
                        return Ok(None);
                    }
                }
                PartEnd::Failed(why) => return Err(why),
            }
        }
        tracing::warn!(
            recipe = %hash, parts = parts.len(),
            "pre-transcode preempted too many times; giving up on this run"
        );
        Ok(None)
    }

    /// Run one part to completion, or until a viewer wants the hardware.
    async fn run_part(
        &self,
        child: &mut Child,
        deadline: Instant,
        yield_to_offline: bool,
        cancelled: Option<&tokio_util::sync::CancellationToken>,
    ) -> PartEnd {
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return PartEnd::Finished,
                Ok(Some(status)) => {
                    return PartEnd::Failed(format!("producer ffmpeg exited with {status}"))
                }
                Ok(None) => {}
                Err(e) => return PartEnd::Failed(format!("waiting on producer ffmpeg: {e}")),
            }
            // Checkpoint and terminate. Not SIGSTOP: a stopped ffmpeg still
            // holds the hardware codec session, so the viewer this is yielding
            // to would be blocked by a process that is doing nothing.
            if self.admissions.live_is_waiting()
                || (yield_to_offline
                    && self
                        .offline_waiting
                        .load(std::sync::atomic::Ordering::Acquire))
            {
                let _ = child.kill().await;
                return PartEnd::Preempted;
            }
            if cancelled.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                let _ = child.kill().await;
                return PartEnd::Preempted;
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                return PartEnd::Deadline;
            }
            tokio::time::sleep(PRODUCER_POLL).await;
        }
    }

    /// Create a session, or hand back the one an identical request already
    /// created.
    ///
    /// The idempotency matters more than it looks. Session creation spawns a
    /// process and kills its predecessor, and it used to be a GET — which is
    /// idempotent *by definition*, so anything in the path that felt entitled
    /// to replay a GET could spawn a second encoder and orphan the first.
    /// Automatic quality switching would have multiplied how often that
    /// mattered. A repeated `request_id` now returns the same session;
    /// the same id asking for something different is a conflict rather than a
    /// quiet second stream.
    ///
    /// The id is *reserved* before any work starts, not checked and then
    /// acted on: the old shape read the map, released the lock, spawned
    /// ffmpeg, and recorded the result — so two concurrent retries with the
    /// same id could both pass the check, spawn two encoders, and leave one
    /// caller holding a session its twin's supersession had already killed.
    /// Now the second caller finds the reservation and waits for the first
    /// one's session instead.
    pub async fn create_session(
        &self,
        req: &SessionRequest,
        user_name: &str,
    ) -> Result<StartInfo, String> {
        let fingerprint = req.fingerprint();
        let claim = match req.request_id.as_deref() {
            Some(key) => match self.claim_request(key, &fingerprint).await? {
                Claimed::Recovered(info) => return Ok(info),
                Claimed::Mine(claim) => Some(claim),
            },
            None => None,
        };

        let info = match req.kind {
            SessionKind::Transcode { height } => {
                self.start_with_audio_offset(
                    req.file_id,
                    height,
                    req.start_seconds,
                    req.audio_index,
                    req.subtitle_burn,
                    req.audio_offset_ms,
                    user_name,
                    &req.playback_id,
                )
                .await?
            }
            SessionKind::Copy {
                aac,
                preserve_dolby_vision,
            } => {
                self.start_copy_with_audio_offset(
                    req.file_id,
                    req.start_seconds,
                    req.audio_index,
                    req.audio_offset_ms,
                    CopySessionOptions {
                        transcode_audio: aac,
                        preserve_dolby_vision,
                    },
                    user_name,
                    &req.playback_id,
                )
                .await?
            }
        };
        if let Some(claim) = claim {
            let live: std::collections::HashSet<String> =
                self.sessions.lock().await.keys().cloned().collect();
            claim.complete(&info.session_id, &live);
        }
        Ok(info)
    }

    /// Resolve a `request_id` to either a reservation this call owns or the
    /// session an identical create already made. Errors on a fingerprint
    /// mismatch (same id, different stream) and on a reservation that outlives
    /// [`INFLIGHT_WAIT`].
    async fn claim_request(&self, key: &str, fingerprint: &str) -> Result<Claimed<'_>, String> {
        let deadline = Instant::now() + INFLIGHT_WAIT;
        loop {
            // What the map says right now, decided under one lock so there is
            // no gap between reading the entry and reserving the key.
            enum Step {
                Reserved,
                Wait,
                Recover(String),
            }
            let step = {
                let mut requests = self.requests.lock().expect("requests mutex");
                match requests.get(key) {
                    Some((seen, _)) if seen != fingerprint => {
                        return Err(format!(
                            "request {key} was already used for a different stream"
                        ));
                    }
                    Some((_, RequestState::InFlight)) => Step::Wait,
                    Some((_, RequestState::Ready(session_id))) => Step::Recover(session_id.clone()),
                    None => {
                        requests.insert(
                            key.to_owned(),
                            (fingerprint.to_owned(), RequestState::InFlight),
                        );
                        Step::Reserved
                    }
                }
            };
            match step {
                Step::Reserved => {
                    return Ok(Claimed::Mine(RequestClaim {
                        requests: &self.requests,
                        key: Some(key.to_owned()),
                        fingerprint: fingerprint.to_owned(),
                    }))
                }
                Step::Recover(session_id) => {
                    if let Some(info) = self.recover(&session_id).await {
                        tracing::debug!(%session_id, request_id = key, "idempotent create: same session");
                        return Ok(Claimed::Recovered(info));
                    }
                    // Its session is gone; the entry is stale, not
                    // authoritative. Remove exactly the entry that was seen —
                    // a peer may have re-reserved the key meanwhile — and
                    // re-decide from the top.
                    let mut requests = self.requests.lock().expect("requests mutex");
                    if matches!(requests.get(key), Some((_, RequestState::Ready(sid))) if *sid == session_id)
                    {
                        requests.remove(key);
                    }
                }
                Step::Wait => {
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "a create with request id {key} has been in flight for over {}s; \
                             assume it died and retry",
                            INFLIGHT_WAIT.as_secs()
                        ));
                    }
                    tokio::time::sleep(INFLIGHT_POLL).await;
                }
            }
        }
    }

    /// Describe a session that already exists, for an idempotent re-create.
    async fn recover(&self, session_id: &str) -> Option<StartInfo> {
        let session = self.sessions.lock().await.get(session_id).cloned()?;
        if session.failed.load(Relaxed) {
            return None;
        }
        let duration_ms = self
            .store
            .get_file(session.file_id)
            .await
            .ok()
            .flatten()
            .and_then(|f| f.duration_ms);
        let encoder = *session.encoder_label.lock().await;
        Some(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id: session_id.to_owned(),
            duration_ms,
            start_seconds: session.start_seconds,
            media_origin_seconds: session.media_origin_seconds,
            encoder,
            vod: session.cached,
        })
    }

    /// A numeric setting, or its default when unset or unparseable.
    async fn num_setting<T>(&self, key: &str, default: T) -> T
    where
        T: std::str::FromStr + PartialOrd + Default,
    {
        match self.store.get_setting(key).await {
            Ok(Some(v)) => v
                .trim()
                .parse::<T>()
                .ok()
                .filter(|n| *n >= T::default())
                .unwrap_or(default),
            _ => default,
        }
    }

    /// A feature switch stored in the ordinary settings table. Only the
    /// literal value `1` enables an experiment; absent, malformed, and every
    /// other value stay on the established path.
    async fn bool_setting(&self, key: &str) -> bool {
        self.store
            .get_setting(key)
            .await
            .ok()
            .flatten()
            .is_some_and(|value| value.trim() == "1")
    }

    /// How an HLS session's input should be paced, given the admin settings
    /// and what this ffmpeg build supports. `for_copy` picks the pre-5.1
    /// degradation (see [`crate::ffmpeg::PacingCaps::resolve`]).
    async fn pacing(&self, for_copy: bool) -> Pacing {
        let rate = self
            .num_setting(keys::HLS_READRATE, HLS_READRATE_DEFAULT)
            .await;
        let burst = self
            .num_setting(keys::HLS_BURST_SECS, HLS_BURST_SECS_DEFAULT)
            .await;
        pacing_caps().await.resolve(rate, burst, for_copy)
    }

    /// Hardware slots in use, and the cap. The pair is the diagnostic: "2"
    /// alone says nothing, and a viewer being refused while the count sits at
    /// zero is a very different bug from one being refused at the cap.
    pub async fn hardware_slots(&self) -> (usize, usize) {
        (self.admissions.in_use(), self.max_hw_sessions().await)
    }

    /// How many hardware transcodes this node will run at once.
    pub async fn max_hw_sessions(&self) -> usize {
        // A configured zero means "no hardware transcoding", which is a
        // legitimate thing to want on a box whose GPU is doing something else.
        self.num_setting(keys::MAX_HW_SESSIONS, DEFAULT_MAX_HW_SESSIONS)
            .await
    }

    /// Threads the software-encoder pool may hand out at once: every core
    /// but one unless the admin says otherwise ([`keys::SW_POOL_THREADS`]).
    /// The tests set the key, so the suite asserts policy rather than the
    /// build machine's core count.
    pub async fn software_budget(&self) -> usize {
        self.num_setting(keys::SW_POOL_THREADS, crate::admission::software_budget())
            .await
    }

    /// Choose the encoder given the admin preference setting (empty = auto).
    async fn encoder(&self) -> Encoder {
        let prefer = self
            .store
            .get_setting(keys::HWACCEL)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        self.caps.choose(&prefer)
    }

    fn rate_control_snapshot(&self) -> RateControlSnapshot {
        *self
            .rate_control
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The effective value a newly normalized session on `encoder` receives.
    pub fn effective_rate_control(&self, encoder: Encoder) -> EffectiveRateControl {
        self.rate_control_snapshot().effective_for(encoder)
    }

    #[cfg(test)]
    pub(crate) fn test_mark_live_waiting(&self) -> crate::admission::LiveWait {
        self.admissions.wait_for_slot()
    }

    async fn validate_rate_control_snapshot(
        &self,
        mode: RateMode,
        quality: Option<u8>,
        policy: RateControlProbePolicy,
    ) -> RateControlValidation {
        if mode == RateMode::Bitrate {
            return RateControlValidation::Complete(RateControlSnapshot {
                requested_mode: mode,
                requested_quality: quality,
                quality_rc: self.caps.quality_rc,
            });
        }

        // Runtime validation is background encoder work, not an exception to
        // the background-work contract. Sharing this gate means it cannot run
        // beside an active speculative or offline encode. If an offline job
        // queues after the probe wins the gate, `offline_waiting` below is its
        // cancellation signal and the probe gives the lane back promptly.
        let _background_lane = match policy {
            RateControlProbePolicy::Boot => None,
            RateControlProbePolicy::YieldingBackground => {
                let Ok(guard) = self.background_producer.try_lock() else {
                    return RateControlValidation::Deferred;
                };
                Some(guard)
            }
        };

        let mut quality_rc = QualityRc::default();
        for encoder in [
            Encoder::Software,
            Encoder::Nvenc,
            Encoder::Qsv,
            Encoder::Vaapi,
            Encoder::VideoToolbox,
        ] {
            if !self.caps.available(encoder) {
                continue;
            }
            let q = quality.unwrap_or_else(|| encoder.default_quality());
            if matches!(policy, RateControlProbePolicy::Boot) {
                quality_rc.set_supported(
                    encoder,
                    transcode::validate_quality_rate_control(
                        &ffmpeg_bin(),
                        encoder,
                        q,
                        self.caps.forced_idr.wanted_by(encoder),
                    )
                    .await,
                );
                continue;
            }
            let should_yield = || {
                self.admissions.live_is_waiting()
                    || self
                        .offline_waiting
                        .load(std::sync::atomic::Ordering::Acquire)
            };
            if should_yield() {
                return RateControlValidation::Deferred;
            }
            let validation = if encoder == Encoder::Software {
                let budget = self.software_budget().await;
                // Even a misconfigured zero-thread budget must account for
                // the process. The software pool's empty-pool exception lets
                // one oversized job run, and a positive weight makes the next
                // live waiter visible so this probe can yield to it.
                let probe_weight = budget.max(1);
                let Some(_permit) =
                    self.admissions
                        .try_admit_software(budget, probe_weight, Priority::Background)
                else {
                    return RateControlValidation::Deferred;
                };
                transcode::validate_quality_rate_control_yielding(
                    &ffmpeg_bin(),
                    encoder,
                    q,
                    self.caps.forced_idr.wanted_by(encoder),
                    should_yield,
                )
                .await
            } else {
                let Some(_slot) = self
                    .admissions
                    .try_acquire(self.max_hw_sessions().await, Priority::Background)
                else {
                    return RateControlValidation::Deferred;
                };
                transcode::validate_quality_rate_control_yielding(
                    &ffmpeg_bin(),
                    encoder,
                    q,
                    self.caps.forced_idr.wanted_by(encoder),
                    should_yield,
                )
                .await
            };
            match validation {
                QualityRateControlValidation::Supported => quality_rc.set_supported(encoder, true),
                QualityRateControlValidation::Refused => quality_rc.set_supported(encoder, false),
                QualityRateControlValidation::Deferred => return RateControlValidation::Deferred,
            }
        }
        RateControlValidation::Complete(RateControlSnapshot {
            requested_mode: mode,
            requested_quality: quality,
            quality_rc,
        })
    }

    fn publish_rate_control(&self, snapshot: RateControlSnapshot, selected: Encoder) {
        let effective = snapshot.effective_for(selected);
        *self
            .rate_control
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        tracing::info!(
            requested_mode = snapshot.requested_mode.as_str(),
            requested_quality = snapshot.requested_quality,
            encoder = selected.label(),
            effective = %effective.snapshot_value(),
            "published validated rate-control snapshot"
        );
    }

    async fn requested_rate_control(
        &self,
    ) -> Result<(RateMode, Option<u8>), plurx_core::error::StoreError> {
        let (raw_mode, raw_quality) = self
            .store
            .get_setting_pair(keys::TRANSCODE_RATE_MODE, keys::TRANSCODE_QUALITY)
            .await?;
        let (mode, quality, corrupt) =
            normalize_rate_control_request(raw_mode.as_deref(), raw_quality.as_deref());
        if corrupt {
            tracing::warn!(
                rate_mode = raw_mode.as_deref().unwrap_or_default(),
                quality = raw_quality.as_deref().unwrap_or_default(),
                "invalid durable rate-control pair — using bitrate"
            );
        }
        Ok((mode, quality))
    }

    async fn refresh_rate_control_locked(
        &self,
    ) -> Result<Option<RateControlSnapshot>, plurx_core::error::StoreError> {
        loop {
            let (mode, quality) = self.requested_rate_control().await?;
            let current = self.rate_control_snapshot();
            if current.requested_mode == mode && current.requested_quality == quality {
                return Ok(Some(current));
            }
            let candidate = match self
                .validate_rate_control_snapshot(
                    mode,
                    quality,
                    RateControlProbePolicy::YieldingBackground,
                )
                .await
            {
                RateControlValidation::Complete(snapshot) => snapshot,
                RateControlValidation::Deferred => return Ok(None),
            };
            // A different node may have committed another complete pair while
            // this node exercised its driver. Publish only a still-current
            // request; otherwise validate the newer pair instead.
            if self.requested_rate_control().await? != (mode, quality) {
                continue;
            }
            let selected = self.encoder().await;
            self.publish_rate_control(candidate, selected);
            return Ok(Some(candidate));
        }
    }

    /// Refresh this node's validated effective state from the replicated
    /// requested pair. Each voter owns different hardware, so requested values
    /// replicate while validation remains deliberately node-local.
    async fn refresh_rate_control(
        &self,
    ) -> Result<Option<RateControlSnapshot>, plurx_core::error::StoreError> {
        let _serial = self.rate_control_update.lock().await;
        self.refresh_rate_control_locked().await
    }

    /// Keep the in-memory hot-path snapshot within the plan's two-second TTL.
    /// Store reads and behavioral probes stay entirely off session creation;
    /// live/producer paths only copy the already-published snapshot.
    pub async fn rate_control_refresh_loop(self: Arc<Self>) {
        let start = tokio::time::Instant::now() + RATE_CONTROL_REFRESH;
        let mut interval = tokio::time::interval_at(start, RATE_CONTROL_REFRESH);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.refresh_rate_control().await {
                Ok(Some(_)) => {}
                Ok(None) => tracing::debug!(
                    "rate-control refresh deferred for live/offline work or occupied encoder capacity"
                ),
                Err(error) => {
                    tracing::warn!(%error, "could not refresh replicated rate-control settings")
                }
            }
        }
    }

    /// Load the durable request at boot, exercise the real production args,
    /// and publish only the effective result. Invalid legacy/corrupt values
    /// fall back to bitrate rather than preventing the server from starting.
    pub async fn initialize_rate_control(&self) -> Result<(), plurx_core::error::StoreError> {
        let _serial = self.rate_control_update.lock().await;
        let (mode, quality) = self.requested_rate_control().await?;
        let RateControlValidation::Complete(snapshot) = self
            .validate_rate_control_snapshot(mode, quality, RateControlProbePolicy::Boot)
            .await
        else {
            unreachable!("boot rate-control validation never defers")
        };
        if self.requested_rate_control().await? == (mode, quality) {
            let selected = self.encoder().await;
            self.publish_rate_control(snapshot, selected);
        } else {
            // Startup settings changed during the probe. The runtime refresher
            // will validate the new complete pair under background admission.
            tracing::info!(
                "rate-control settings changed during boot validation; deferring refresh"
            );
        }
        Ok(())
    }

    /// Validate and durably apply one complete requested setting pair.
    /// Sessions keep the old effective snapshot until every probe and both
    /// writes succeed, then all new sessions see the new one at once.
    pub async fn apply_rate_control_settings(
        &self,
        mode: RateMode,
        quality: Option<u8>,
    ) -> Result<(), ApplyRateControlError> {
        let _serial = self.rate_control_update.lock().await;
        let snapshot = match self
            .validate_rate_control_snapshot(
                mode,
                quality,
                RateControlProbePolicy::YieldingBackground,
            )
            .await
        {
            RateControlValidation::Complete(snapshot) => snapshot,
            RateControlValidation::Deferred => return Err(ApplyRateControlError::Busy),
        };
        let stored_quality = quality.map(|value| value.to_string()).unwrap_or_default();
        self.store
            .put_settings(&[
                (keys::TRANSCODE_QUALITY, stored_quality.as_str()),
                (keys::TRANSCODE_RATE_MODE, mode.as_str()),
            ])
            .await?;
        if self.requested_rate_control().await? == (mode, quality) {
            let selected = self.encoder().await;
            self.publish_rate_control(snapshot, selected);
        } else {
            // A concurrent writer on another voter won after our transaction.
            // Bring this process to the replicated winner before returning
            // when a safe probe window exists. If it does not, the write has
            // already completed and must not be mislabeled as the pre-write
            // Busy/409 case; the background loop will retry while the HTTP
            // response reports the replicated winner's requested pair.
            if self.refresh_rate_control_locked().await?.is_none() {
                tracing::info!(
                    "concurrent rate-control winner will be validated by the background refresher"
                );
            }
        }
        Ok(())
    }

    /// The rung "Auto" means, given what this server can actually encode with.
    ///
    /// Auto was 720p for everything, which was the right answer when every
    /// transcode was software: 1080p on x264 is a session that cannot hold
    /// realtime on a NUC, and a stream that stutters at 1080p is worse than one
    /// that plays at 720p. A hardware encoder changes the arithmetic, not the
    /// reasoning — it clears realtime at 1080p comfortably — so Auto follows
    /// the source up to 1080p when there is one and keeps the conservative
    /// answer when there is not.
    ///
    /// Capped at 1080 on purpose rather than at the source: a 4K rung is a
    /// bandwidth decision as much as a CPU one, and Auto should not put 20 Mb/s
    /// on somebody's Wi-Fi without being asked. 4K stays a menu choice, and
    /// direct play and remux still deliver the source untouched. Never
    /// upscales: a 480p source transcodes at 480p.
    ///
    /// The server decides this, not the player, because only the server knows
    /// which encoder won — the player learns that from the response it gets
    /// back *after* the height has been chosen (PERF-PLAN §4.7).
    pub async fn auto_height(&self, source_height: Option<i64>) -> i64 {
        // Software's ceiling is lower; the shape is the same. It used to
        // return its ceiling unconditionally, which never *upscaled* — the
        // filter caps at the source — but advertised 720p bitrate and
        // response metadata for a 480p stream (review §3.1). The rung is a
        // promise about the output; it follows the source on both encoders.
        let max = if self.encoder().await == Encoder::Software {
            AUTO_SOFTWARE_HEIGHT
        } else {
            AUTO_HARDWARE_MAX_HEIGHT
        };
        source_height
            .filter(|h| *h > 0)
            .unwrap_or(max)
            .clamp(MIN_HEIGHT, max)
    }

    /// The admin's playback language preferences (Settings → Playback
    /// defaults), falling back to English/English/Auto.
    pub async fn lang_prefs(&self) -> plurx_core::tracks::LangPrefs {
        let mut prefs = plurx_core::tracks::LangPrefs::default();
        if let Ok(Some(v)) = self.store.get_setting(keys::AUDIO_LANG).await {
            if !v.trim().is_empty() {
                prefs.audio_lang = v.trim().to_owned();
            }
        }
        if let Ok(Some(v)) = self.store.get_setting(keys::SUB_LANG).await {
            if !v.trim().is_empty() {
                prefs.sub_lang = v.trim().to_owned();
            }
        }
        if let Ok(Some(v)) = self.store.get_setting(keys::SUB_MODE).await {
            prefs.sub_mode = plurx_core::tracks::SubMode::parse(v.trim());
        }
        prefs
    }

    /// Kill any session belonging to the same player instance.
    ///
    /// A seek is a *new* session: the client asks for a playlist starting at the
    /// new position and abandons the old one without telling anyone. Nothing in
    /// the protocol says the old session is finished, so it was left to the idle
    /// reaper — 60s idle, noticed by a 15s ticker, so up to ~75 seconds of a
    /// second ffmpeg still writing segments nobody will fetch. Scrub along a
    /// timeline and those stack: ten seeks in a minute is ten encoders (or ten
    /// remuxes reading the source flat-out), all competing for the same CPU,
    /// GPU, disk and link. That is enough on its own to starve a Wi-Fi client
    /// badly enough to cost it its DHCP lease.
    ///
    /// The key is the *player instance*, not the viewer. It used to be
    /// (viewer, file), which made two devices signed in to one account fight
    /// over the same film — each new session killing the other's — and
    /// automatic quality restarts would have turned that from a rare
    /// annoyance into a loop. A player instance restarts its own stream all
    /// the time and never anyone else's, which is exactly the scope wanted.
    async fn reap_superseded(&self, playback_id: &str) {
        let doomed: Vec<(String, Arc<Session>)> = {
            let mut sessions = self.sessions.lock().await;
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| s.playback_id == playback_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id).map(|s| (id, s)))
                .collect()
        };
        for (session_id, session) in doomed {
            // Before the kill, and before the caller goes on to ask for a slot
            // of its own: a player replacing its own session must not have to
            // queue behind the session it just replaced — on either pool.
            session.release_hardware();
            session.release_software();
            session.kill_child().await;
            session.discard_dir().await;
            self.emit_session_event(
                &session_id,
                &session,
                "session_end",
                SessionEventFields {
                    reason: Some("superseded"),
                    ..SessionEventFields::default()
                },
            )
            .await;
            tracing::info!(
                %session_id, playback_id,
                "reaped superseded transcode session (this player started a new one)"
            );
        }
    }

    /// Start a transcode session for a file, superseding this viewer's previous
    /// session on the same file (see [`Self::reap_superseded`]).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)] // one stream's worth of knobs
    pub async fn start(
        &self,
        file_id: i64,
        target_height: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        subtitle_override: Option<i64>,
        user_name: &str,
        playback_id: &str,
    ) -> Result<StartInfo, String> {
        self.start_with_audio_offset(
            file_id,
            target_height,
            start_seconds,
            audio_override,
            subtitle_override,
            0,
            user_name,
            playback_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // one stream's worth of knobs
    async fn start_with_audio_offset(
        &self,
        file_id: i64,
        target_height: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        subtitle_override: Option<i64>,
        audio_offset_ms: i64,
        user_name: &str,
        playback_id: &str,
    ) -> Result<StartInfo, String> {
        let rate_control = self.rate_control_snapshot();
        // Before spawning, not after: the point is to never have two encoders
        // for one player running at once, and reaping first also frees the
        // hardware slot the new session is about to want.
        self.reap_superseded(playback_id).await;

        let mut file = self
            .store
            .get_file(file_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_owned())?;
        file.audio_offset_ms = if file.audio_streams.is_empty() {
            0
        } else {
            audio_offset_ms.clamp(-15_000, 15_000)
        };
        let item_title = self
            .store
            .get_item(file.item_id)
            .await
            .ok()
            .flatten()
            .map(|i| i.title)
            .unwrap_or_else(|| "(unknown)".to_owned());

        // Which tracks this session carries. Before the hardware slot, because
        // the cache lookup below needs the answer — two sessions differing only
        // in audio track are different bytes, and a cache that ignored that
        // would serve the wrong language.
        let Tracks {
            audio_index,
            subtitle_burn,
        } = self
            .select_tracks(&file, audio_override, subtitle_override)
            .await;

        // The cache, before anything is claimed. A hit needs no encoder, no
        // hardware slot and no place in the queue — the work is already done,
        // and making a viewer wait behind a busy GPU for bytes that exist is
        // the one thing this cache exists to prevent.
        let mut encoder = self.encoder().await;
        let mut opts = self.options_for(
            encoder,
            &file,
            target_height,
            start_seconds,
            audio_index,
            subtitle_burn.clone(),
            None,
        );
        opts.effective_rate_control = rate_control.effective_for(encoder);
        if let Some(info) = self
            .serve_cached(&file, &opts, encoder, &item_title, user_name, playback_id)
            .await
        {
            return Ok(info);
        }
        self.ensure_text_subtitle(&file, subtitle_burn.as_ref())
            .await?;

        // Claim a hardware slot before spawning anything. An iGPU has one
        // video-processing block, and a third 4K session on it does not run a
        // third as fast — it drags the other two under realtime with it, so one
        // person pressing play becomes three people stuttering.
        //
        // The wait is short and deliberate: a slot usually frees within seconds
        // (a superseded session, a closed tab), and someone who has pressed
        // play will forgive five seconds far sooner than a hang.
        let work = Workload::of(&file, target_height);
        let mut hw_slot = None;
        let mut sw_permit = None;
        let sw_budget = self.software_budget().await;
        if encoder != Encoder::Software {
            let max = self.max_hw_sessions().await;
            // Announced for the whole wait, including the first attempt. A
            // producer holding the slot this start wants sees the announcement,
            // checkpoints and terminates; and while the announcement stands it
            // cannot take the slot back. The guard is dropped by every exit
            // from this block, so a start that fails or gives up does not park
            // background work for the life of the process.
            let _queued = self.admissions.wait_for_slot();
            let deadline = Instant::now() + QUEUE_WAIT;
            loop {
                match self.admissions.admit(max, work) {
                    Admission::Hardware(slot) => {
                        hw_slot = Some(slot);
                        break;
                    }
                    _ if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    // Out of patience. Software only when this box has measured
                    // this *class of work* running comfortably above realtime —
                    // never because the output is small, which is the reasoning
                    // that admits a 4K HDR source at 480p and then stalls: the
                    // decode and the tone-map happen at source resolution
                    // whatever size you ask the output to be. And only with a
                    // permit from the CPU pool: measured-fast is a claim about
                    // one session on an otherwise-idle box, not about joining
                    // three others already spending every core (§2.4).
                    Admission::Software => {
                        match self.admissions.try_admit_software(
                            sw_budget,
                            work.software_threads(),
                            Priority::Live,
                        ) {
                            Some(permit) => {
                                tracing::info!(
                                    file = file_id, class = %work.software_class(),
                                    threads = permit.threads(),
                                    "hardware transcode slots full; this class runs comfortably                                      in software here, so starting it there"
                                );
                                encoder = Encoder::Software;
                                sw_permit = Some(permit);
                                break;
                            }
                            None => {
                                let why = format!(
                                    "all {max} hardware transcode slots are in use and the                                      software CPU pool is spent ({} of {sw_budget} threads                                      reserved). Try again in a moment.",
                                    self.admissions.software_in_use()
                                );
                                tracing::warn!(file = file_id, class = %work.software_class(), "{why}");
                                return Err(why);
                            }
                        }
                    }
                    Admission::Refused(why) => {
                        tracing::warn!(file = file_id, class = %work.software_class(), "{why}");
                        return Err(why);
                    }
                }
            }
        } else {
            // Software from the start — a box with no hardware encoder. This
            // path used to bypass admission entirely: every x264 process
            // chose its own thread count, and nothing stopped a fourth
            // session from joining three that already had every core spoken
            // for (§2.4). The wait is announced exactly like a hardware
            // queue, so the producer checkpoints out of the way of a viewer
            // here too.
            let _queued = self.admissions.wait_for_slot();
            let deadline = Instant::now() + QUEUE_WAIT;
            loop {
                if let Some(permit) = self.admissions.try_admit_software(
                    sw_budget,
                    work.software_threads(),
                    Priority::Live,
                ) {
                    sw_permit = Some(permit);
                    break;
                }
                if Instant::now() >= deadline {
                    let why = format!(
                        "the software CPU pool is spent ({} of {sw_budget} threads reserved)                          and no slot freed within {}s. Try again in a moment.",
                        self.admissions.software_in_use(),
                        QUEUE_WAIT.as_secs()
                    );
                    tracing::warn!(file = file_id, class = %work.software_class(), "{why}");
                    return Err(why);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("creating session dir: {e}"))?;

        // Admission may have moved this session to software, which changes the
        // pipeline it is entitled to — so the options are rebuilt rather than
        // patched. Same builder, so the two cannot describe different sessions.
        // The software permit's thread budget rides in the same rebuild: what
        // admission reserved is exactly what x264 is told to spend.
        let mut opts = self.options_for(
            encoder,
            &file,
            target_height,
            start_seconds,
            audio_index,
            subtitle_burn,
            sw_permit.as_ref().map(|p| p.threads() as u32),
        );
        opts.effective_rate_control = rate_control.effective_for(encoder);
        let pacing = self.pacing(false).await;
        let typeless_sliding = self.bool_setting(keys::HLS_TYPELESS_SLIDING).await;
        let args = transcode::hls_args(&file, encoder, &opts, pacing, &dir.to_string_lossy());
        // Log the exact command — the single most useful diagnostic. It reveals
        // the decode/filter/encode pipeline actually used (e.g. whether heavy
        // HEVC is being hardware-decoded), and confirms which build is running.
        //
        // And, when this session did not get the graph the node proved, why
        // not. Without it `pipeline=cpu` on a 4K HDR title reads as the GPU
        // path being broken, when the usual answer is that the source is Dolby
        // Vision and the CPU chain is the *correct* choice.
        let declined = Pipeline::declined(
            self.pipeline,
            encoder,
            transcode::routing_hdr(&file),
            transcode::heavy_source(&file),
            opts.subtitle_burn.as_ref().is_some_and(|b| !b.bitmap),
        );
        tracing::info!(
            %session_id, encoder = encoder.label(), pipeline = opts.pipeline.name(),
            proven = self.pipeline.name(), hdr = file.hdr.as_deref().unwrap_or("sdr"),
            declined = declined.unwrap_or(""),
            build = crate::version::BUILD,
            "transcode ffmpeg args: {}", args.join(" ")
        );
        let progress = Arc::new(Progress::new());
        let generation = progress.begin_attempt();
        let child = spawn_ffmpeg(
            &args,
            encoder.label(),
            &session_id,
            Arc::clone(&progress),
            generation,
            &self.runtime_cache,
        )?;

        tracing::info!(
            %session_id, file_id, target_height, start_seconds,
            encoder = encoder.label(), "started transcode session"
        );

        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(Some(child)),
            cached: false,
            _cache_reader: None,
            last_request: Mutex::new(LastRequest::now("session-start")),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            method: crate::delivery::Method::Transcode,
            start_seconds,
            // A transcode seeks accurately, so its media begins exactly where
            // it was asked to: no probe, no discrepancy to resolve.
            media_origin_seconds: start_seconds,
            hls_codecs: "avc1.640034,mp4a.40.2".into(),
            hls_supplemental_codecs: None,
            target_height,
            encoder_label: Mutex::new(encoder.label()),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            playlist_published: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            live_bytes: AtomicI64::new(0),
            progress: Arc::clone(&progress),
            class: std::sync::Mutex::new(work.class(if encoder == Encoder::Software {
                crate::admission::SOFTWARE
            } else {
                encoder.label()
            })),
            hw_slot: std::sync::Mutex::new(hw_slot),
            sw_permit: std::sync::Mutex::new(sw_permit),
            delivery: Meter::new(),
            readrate: pacing
                .readrate
                .unwrap_or(if pacing.legacy_re { 1.0 } else { 0.0 }),
            suspended: AtomicBool::new(false),
            suspended_at: Mutex::new(None),
            suspend_count: AtomicU64::new(0),
            typeless_sliding,
            first_slide_logged: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));
        self.emit_session_event(
            &session_id,
            &session,
            "session_start",
            SessionEventFields {
                extra: Some(serde_json::json!({ "cache": "miss" }).to_string()),
                ..SessionEventFields::default()
            },
        )
        .await;

        // A hardware path can init cleanly yet produce nothing — GPU contention
        // under a second session, or a decode the GPU can't do (a 4K Dolby
        // Vision HEVC stream is the classic case). Watch the *playlist* for a
        // finished segment; if none lands in the grace window, restart on
        // software. If software also can't produce a first segment in its
        // (longer) window, mark the session failed so the client gets an error
        // instead of a gray screen forever. Software-started sessions still get
        // the fail-fast guard, just not the hardware→software step.
        {
            let session = Arc::clone(&session);
            let file = file.clone();
            let opts = opts.clone();
            let dir = dir.clone();
            let sid = session_id.clone();
            let started_on_hardware = encoder != Encoder::Software;
            let sw_pool = self.admissions.software_pool();
            let runtime_cache = self.runtime_cache.clone();
            let software_rate_control = rate_control.effective_for(Encoder::Software);
            tokio::spawn(async move {
                if started_on_hardware {
                    tokio::time::sleep(FIRST_SEGMENT_GRACE).await;
                    // A watch, not a glance. The single verdict this used to
                    // take had three ways to walk away early — producing,
                    // suspended, still advancing — and every one of them ended
                    // monitoring for the life of the session, so a pipeline
                    // that wedged a minute in was nobody's problem (§2.3).
                    // Now the only exits are into the lifetime watchdog below,
                    // or through the one-step downgrade — which still fires at
                    // most once per session, on the same evidence as before:
                    // no playable segment AND output stopped advancing.
                    let mut announced_slow = false;
                    loop {
                        if session.failed.load(Relaxed) {
                            return;
                        }
                        if session_producing(&dir).await {
                            // Producing real segments. If the picture is still
                            // gray, the problem is the *output* (tone-map/
                            // color), not the pipeline stalling — this line
                            // says which. The speed says how much headroom it
                            // has while doing it.
                            tracing::info!(
                                session = %sid,
                                speed = session.progress.speed(),
                                "transcode producing segments (hardware path healthy)"
                            );
                            break;
                        }
                        let exited = {
                            let mut child = session.child.lock().await;
                            child
                                .as_mut()
                                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
                        };
                        if exited {
                            // Died before producing: the playlist and segment
                            // readers report that; there is nothing left here
                            // to downgrade.
                            return;
                        }
                        // Producing nothing, but *is* it stuck? A session
                        // stopped for running ahead obviously makes no
                        // progress, and one decoding 4K at 0.4x is slow rather
                        // than broken — restarting either on software trades a
                        // slow start for a slower one. Only a session whose
                        // output has actually stopped moving gets the
                        // fallback; the rest keep being watched rather than
                        // walked away from.
                        let suspended = session.suspended.load(Relaxed);
                        if suspended || session.progress.stalled_for() < PROGRESS_STALL {
                            if !suspended && !announced_slow {
                                announced_slow = true;
                                tracing::info!(
                                    session = %sid,
                                    produced_ms = session.progress.out_time_ms(),
                                    speed = session.progress.speed(),
                                    "no finished segment yet, but the encoder is still \
                                     advancing — watching it rather than restarting it on \
                                     something slower"
                                );
                            }
                            tokio::time::sleep(WATCHDOG_POLL).await;
                            continue;
                        }
                        break;
                    }
                    // Fell out of the loop two ways: producing (nothing to
                    // fix — skip straight to the lifetime watchdog), or
                    // stalled without a segment (downgrade one step).
                    if !session_producing(&dir).await {
                        Self::downgrade_one_step(
                            &session,
                            &file,
                            &opts,
                            encoder,
                            software_rate_control,
                            pacing,
                            &sw_pool,
                            &dir,
                            &sid,
                            &runtime_cache,
                        )
                        .await;
                        if session.failed.load(Relaxed) {
                            return;
                        }
                    }
                }

                watch_for_stall(session, dir, sid).await;
            });
        }

        Ok(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id,
            duration_ms: file.duration_ms,
            start_seconds,
            media_origin_seconds: start_seconds,
            encoder: encoder.label(),
            vod: false,
        })
    }

    /// Downgrade a stalled hardware start by ONE step, taking the cheaper
    /// step first.
    ///
    /// A session running a GPU tone-map graph that stopped producing is
    /// evidence about the graph — a driver state the boot probe couldn't
    /// reach, a codec profile its fixture didn't cover, another session
    /// contending for the same block. None of that is evidence about the
    /// *encoder*, and swapping to software would trade a stalled hardware
    /// session for one that is slower still. So the graph goes and the
    /// hardware stays; only a session already on the CPU chain falls back to
    /// a software encoder.
    ///
    /// One step per session, deliberately: this fires once, and a viewer who
    /// has waited out the grace window twice has waited too long. If the
    /// downgraded session also stalls, the lifetime watchdog takes it. On a
    /// spawn failure the session is marked failed; the caller checks.
    #[allow(clippy::too_many_arguments)] // one fallback's worth of context
    async fn downgrade_one_step(
        session: &Session,
        file: &plurx_core::domain::MediaFile,
        opts: &TranscodeOptions,
        encoder: Encoder,
        software_rate_control: EffectiveRateControl,
        pacing: Pacing,
        sw_pool: &crate::admission::SwPool,
        dir: &std::path::Path,
        sid: &str,
        runtime_cache: &std::path::Path,
    ) -> EffectiveRateControl {
        let downgrade_pipeline = opts.pipeline.on_gpu();
        let retry_encoder = if downgrade_pipeline {
            encoder
        } else {
            Encoder::Software
        };
        let mut retry_opts = opts.clone();
        if downgrade_pipeline {
            retry_opts.pipeline = opts.pipeline.fallback().unwrap_or(Pipeline::Cpu);
        } else {
            // A family-tuned default is part of the effective identity. A
            // VideoToolbox value, for example, is not a valid x264 CRF merely
            // because both are integers.
            retry_opts.effective_rate_control = software_rate_control;
        }
        tracing::warn!(
            session = %sid,
            stalled_s = session.progress.stalled_for().as_secs(),
            pipeline = opts.pipeline.name(),
            retry_pipeline = retry_opts.pipeline.name(),
            retry_encoder = retry_encoder.label(),
            "no HLS segment from hardware and output has stopped \
             advancing (GPU contention, or a decode the GPU can't do — e.g. \
             Dolby Vision); {}",
            if downgrade_pipeline {
                "dropping the GPU tone-map and keeping the hardware encoder"
            } else {
                "retrying on software"
            }
        );
        session.kill_child().await;
        clear_session_dir(dir).await;
        if !downgrade_pipeline {
            // The slot belonged to the encoder that just died, not
            // to the session: hand it back at the transition so
            // the next hardware start gets it now, and re-class
            // the admission record for the software encoder that
            // is about to be measured. The replacement takes its CPU
            // pool share by force — a viewer already watching is not
            // held hostage to the budget — and spends exactly what it
            // reserved, as an explicit -threads.
            let work = Workload::of(file, session.target_height);
            let permit = sw_pool.take_forced(work.software_threads());
            retry_opts.software_threads = Some(permit.threads() as u32);
            session.demote_to_software(work, permit);
        }
        let sw_args = transcode::hls_args(
            file,
            retry_encoder,
            &retry_opts,
            pacing,
            &dir.to_string_lossy(),
        );
        // The replacement writes its own timeline from the same
        // seek point; keeping the dead process's telemetry would
        // make it look stalled from its first second, and the
        // generation bump is what stops the dead process's reader
        // from writing those numbers back after the reset.
        let generation = session.progress.begin_attempt();
        match spawn_ffmpeg(
            &sw_args,
            retry_encoder.label(),
            sid,
            Arc::clone(&session.progress),
            generation,
            runtime_cache,
        ) {
            Ok(child) => {
                *session.child.lock().await = Some(child);
                *session.last_request.lock().await = LastRequest::now("fallback-start");
                // The activity page must stop naming the hardware
                // encoder the moment it is no longer the one running.
                *session.encoder_label.lock().await = retry_encoder.label();
                tracing::info!(
                    session = %sid,
                    encoder = retry_encoder.label(),
                    pipeline = retry_opts.pipeline.name(),
                    "fallback transcode started"
                );
            }
            Err(e) => {
                tracing::error!(session = %sid, "fallback transcode failed: {e}");
                session.failed.store(true, Relaxed);
            }
        }
        retry_opts.effective_rate_control
    }

    /// Start a **copy-video** HLS session: the source video is repackaged into
    /// HLS (fMP4 segments) untouched, and only the audio is transcoded when the
    /// client can't take it. This is the remux path for players whose `<video>`
    /// won't accept a progressive fragmented MP4 (Safari) but decode HEVC/HDR
    /// natively via HLS — so the original 4K stream is preserved instead of the
    /// error-fallback re-encoding it down to 720p. No hardware/software encoder
    /// ladder (nothing is encoded), just a fail-fast guard.
    #[cfg(test)]
    async fn start_copy(
        &self,
        file_id: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        options: CopySessionOptions,
        user_name: &str,
        playback_id: &str,
    ) -> Result<StartInfo, String> {
        self.start_copy_with_audio_offset(
            file_id,
            start_seconds,
            audio_override,
            0,
            options,
            user_name,
            playback_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // one stream's worth of knobs
    async fn start_copy_with_audio_offset(
        &self,
        file_id: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        audio_offset_ms: i64,
        options: CopySessionOptions,
        user_name: &str,
        playback_id: &str,
    ) -> Result<StartInfo, String> {
        // Same reasoning as `start`; the copy path matters more if anything,
        // since an abandoned remux reads the source as fast as the disk allows.
        self.reap_superseded(playback_id).await;

        let mut file = self
            .store
            .get_file(file_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_owned())?;
        let probe_json = self.store.get_file_probe_json(file_id).await.ok().flatten();
        file.audio_offset_ms = if file.audio_streams.is_empty() {
            0
        } else {
            audio_offset_ms.clamp(-15_000, 15_000)
        };
        // Copy-video sessions and transcodes must interpret an omitted audio
        // override identically. `/decision` marks the shared-policy pick as
        // default, so silently falling back to ffmpeg's first stream here can
        // make the player show English while the session carries (for example)
        // an Italian container default. An explicit viewer choice still wins.
        let audio_index = self
            .select_tracks(&file, audio_override, None)
            .await
            .audio_index;
        let item_title = self
            .store
            .get_item(file.item_id)
            .await
            .ok()
            .flatten()
            .map(|i| i.title)
            .unwrap_or_else(|| "(unknown)".to_owned());

        let session_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("creating session dir: {e}"))?;

        // An ffmpeg capability, read from the daemon's own record of which
        // ffmpeg it runs. It used to be read off the CACHE config — which
        // carries a copy of the same string — so a node with no cache
        // configured silently answered "no dovi_rpu" whatever it was running,
        // left the DV configuration in every remux, and had Chrome refuse the
        // stream Safari played fine.
        let have_dovi = self.dv_strippable();
        let pacing = self.pacing(true).await;
        let typeless_sliding = self.bool_setting(keys::HLS_TYPELESS_SLIDING).await;
        let legacy_args = || {
            transcode::hls_copy_args_with_dolby_vision(
                &file,
                start_seconds,
                audio_index,
                options.transcode_audio,
                pacing,
                transcode::DolbyVisionCopyOptions::new(have_dovi, options.preserve_dolby_vision),
                &dir.to_string_lossy(),
            )
        };
        let progress = Arc::new(Progress::new());
        let generation = progress.begin_attempt();

        // Take over the cutting when the source is one whose keyframes can be
        // read (docs/SEGMENTER-PLAN.md). ffmpeg then writes one continuous
        // fragmented stream down a pipe and `copyseg` decides where the
        // segments end — in front of a keyframe no player will discard a
        // leading picture at. If anything about that stream turns out to be
        // unreadable, the reader task respawns this same session on the
        // arguments below, so the worst case is exactly today's behaviour.
        let segmenting = copyseg::supports(file.video_codec.as_deref());
        let (child, pipe_stdout) = if segmenting {
            let args = transcode::copy_pipe_args_with_dolby_vision(
                &file,
                start_seconds,
                audio_index,
                options.transcode_audio,
                pacing,
                have_dovi,
                options.preserve_dolby_vision,
            );
            tracing::info!(
                %session_id, file_id, start_seconds, mode = "segmenter",
                build = crate::version::BUILD,
                "copy-video HLS ffmpeg args: {}", args.join(" ")
            );
            match spawn_ffmpeg_pipe(
                &args,
                &session_id,
                Arc::clone(&progress),
                generation,
                &self.runtime_cache,
            ) {
                Ok((child, stdout)) => (child, Some(stdout)),
                Err(e) => {
                    // Spawning failed before any of this was decided, so there
                    // is nothing to unwind: start the legacy path here.
                    tracing::warn!(
                        %session_id,
                        "copy segmenter could not start ffmpeg ({e}); using the HLS muxer"
                    );
                    let args = legacy_args();
                    let child = spawn_ffmpeg(
                        &args,
                        "copy",
                        &session_id,
                        Arc::clone(&progress),
                        generation,
                        &self.runtime_cache,
                    )?;
                    (child, None)
                }
            }
        } else {
            let args = legacy_args();
            tracing::info!(
                %session_id, file_id, start_seconds, mode = "legacy",
                build = crate::version::BUILD,
                "copy-video HLS ffmpeg args: {}", args.join(" ")
            );
            let child = spawn_ffmpeg(
                &args,
                "copy",
                &session_id,
                Arc::clone(&progress),
                generation,
                &self.runtime_cache,
            )?;
            (child, None)
        };

        // Deliberately after the spawn: ffmpeg is already opening the source
        // while this runs, so the probe costs the viewer nothing it was not
        // already waiting for.
        let media_origin_seconds = probe_media_origin(&file.path, start_seconds).await;

        let (hls_codecs, hls_supplemental_codecs) =
            copied_hls_codecs(&file, audio_index, options, probe_json.as_deref());
        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(Some(child)),
            cached: false,
            _cache_reader: None,
            last_request: Mutex::new(LastRequest::now("session-start")),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            method: crate::delivery::Method::HlsCopy,
            start_seconds,
            media_origin_seconds,
            hls_codecs,
            hls_supplemental_codecs,
            target_height: file.height.unwrap_or(0),
            encoder_label: Mutex::new("copy"),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            playlist_published: AtomicBool::new(true),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            live_bytes: AtomicI64::new(0),
            progress: Arc::clone(&progress),
            class: std::sync::Mutex::new(String::new()),
            hw_slot: std::sync::Mutex::new(None),
            sw_permit: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            readrate: pacing
                .readrate
                .unwrap_or(if pacing.legacy_re { 1.0 } else { 0.0 }),
            suspended: AtomicBool::new(false),
            suspended_at: Mutex::new(None),
            suspend_count: AtomicU64::new(0),
            typeless_sliding,
            first_slide_logged: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));
        self.emit_session_event(
            &session_id,
            &session,
            "session_start",
            SessionEventFields::default(),
        )
        .await;

        // The reader task, when plurx is doing the cutting. It owns the pipe
        // for the session's life, and it owns the one fallback: a stream it
        // cannot follow is killed and respawned on ffmpeg's HLS muxer, in this
        // same session, so the player never learns anything happened. Only
        // while the playlist is unpublished — once it is out, a respawn would
        // rewrite a timeline a player may already be holding. (The publish
        // gate keeps that window open through the first few segments: files
        // nothing was ever told about are cleared and recut, not a timeline.)
        if let Some(stdout) = pipe_stdout {
            let session = Arc::clone(&session);
            let sid = session_id.clone();
            let dir = dir.clone();
            let file = file.clone();
            let progress = Arc::clone(&progress);
            let runtime_cache = self.runtime_cache.clone();
            tokio::spawn(async move {
                let outcome =
                    copyseg::run(stdout, dir.clone(), &sid, copyseg::Limits::default()).await;
                match outcome {
                    copyseg::Outcome::Ran(counts) => {
                        tracing::info!(
                            session = %sid, build = crate::version::BUILD,
                            "{}", copyseg::summary(&counts)
                        );
                    }
                    copyseg::Outcome::Unsupported(reason) => {
                        // Is this session still one anybody is watching?
                        //
                        // The reader task holds its own `Arc<Session>`, so the
                        // struct outlives removal from the manager's map —
                        // respawning into a stopped, superseded or reaped
                        // session would put an ffmpeg behind a session id
                        // nothing schedules, suspends or reaps, reading the
                        // source flat out through its 90 s burst until the
                        // last Arc drops. And it would log a warning blaming
                        // the SOURCE for what was a viewer pressing stop,
                        // which is how a good path gets a bad reputation.
                        //
                        // The session directory is the test, because every
                        // teardown path removes it (`Session::discard_dir`)
                        // and `failed` is set by none of them.
                        if session.failed.load(Relaxed) || tokio::fs::metadata(&dir).await.is_err()
                        {
                            return;
                        }
                        tracing::warn!(
                            session = %sid,
                            "copy segmenter cannot read this stream ({reason}); \
                             falling back to ffmpeg's HLS muxer for this session"
                        );
                        session.kill_child().await;
                        clear_session_dir(&dir).await;
                        let args = transcode::hls_copy_args_with_dolby_vision(
                            &file,
                            start_seconds,
                            audio_index,
                            options.transcode_audio,
                            pacing,
                            transcode::DolbyVisionCopyOptions::new(
                                have_dovi,
                                options.preserve_dolby_vision,
                            ),
                            &dir.to_string_lossy(),
                        );
                        // A fresh generation, or the dead process's last
                        // progress blocks land on the replacement's telemetry
                        // and the watchdog reads it as stalled from birth.
                        // (The replacement then gets PROGRESS_STALL rather
                        // than the full SOFTWARE_GRACE to emit its first
                        // block, exactly as the hardware→software ladder's
                        // replacement does. It matters only for a fallback
                        // taken tens of seconds in, which means ffmpeg never
                        // produced a moov — a session already in trouble.)
                        let generation = progress.begin_attempt();
                        match spawn_ffmpeg(
                            &args,
                            "copy",
                            &sid,
                            progress,
                            generation,
                            &runtime_cache,
                        ) {
                            Ok(child) => {
                                *session.child.lock().await = Some(child);
                                *session.last_request.lock().await =
                                    LastRequest::now("fallback-start");
                                tracing::info!(session = %sid, "fallback copy started");
                            }
                            Err(e) => {
                                tracing::error!(session = %sid, "fallback copy failed: {e}");
                                session.failed.store(true, Relaxed);
                            }
                        }
                    }
                }
            });
        }

        // Fail-fast guard: copy has no encoder ladder, but if the segments stop
        // coming (undecodable source, ffmpeg refusal) mark the session failed
        // so the player errors instead of waiting on a gray screen.
        {
            let session = Arc::clone(&session);
            let dir = dir.clone();
            let sid = session_id.clone();
            tokio::spawn(async move { watch_for_stall(session, dir, sid).await });
        }

        Ok(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id,
            duration_ms: file.duration_ms,
            start_seconds,
            media_origin_seconds,
            encoder: "copy",
            vod: false,
        })
    }

    /// Number of live transcode sessions (for /metrics).
    pub async fn active_sessions(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Every live session, paired with how it is really delivering.
    ///
    /// The pair is what the activity array needs and `SessionInfo` cannot
    /// give it: a copy-remux and a transcode are the same struct, and the one
    /// field that ever hinted at the difference — `encoder` — is a label, not
    /// a kind. It reads "cached" on a cache hit and is *rewritten* under the
    /// hardware→software fallback, so a page inferring the method from it
    /// would relabel a stream mid-play. The method is fixed when the session
    /// is created and never moves.
    pub async fn list_deliveries(&self) -> Vec<(SessionInfo, crate::delivery::Method)> {
        let limits = self.ahead_limits().await;
        let (global_live_bytes, global_ahead_bytes) = self.global_flow_bytes().await;
        let sessions = self.sessions.lock().await;
        let mut out = Vec::with_capacity(sessions.len());
        for (id, s) in sessions.iter() {
            out.push((
                session_info(id, s, limits, global_live_bytes, global_ahead_bytes).await,
                s.method,
            ));
        }
        out.sort_by_key(|(s, _)| s.started_unix);
        out
    }

    /// One session's live telemetry, for the player's stats overlay. Does not
    /// touch the last-request clock: asking how a stream is doing is not the same as
    /// fetching from it, and a status poll must not keep an abandoned session
    /// alive past the idle reaper.
    pub async fn session_status(&self, session_id: &str) -> Option<SessionInfo> {
        let session = self.sessions.lock().await.get(session_id).cloned()?;
        let limits = self.ahead_limits().await;
        let (global_live_bytes, global_ahead_bytes) = self.global_flow_bytes().await;
        Some(
            session_info(
                session_id,
                &session,
                limits,
                global_live_bytes,
                global_ahead_bytes,
            )
            .await,
        )
    }

    async fn emit_session_event(
        &self,
        session_id: &str,
        session: &Session,
        event: &str,
        fields: SessionEventFields<'_>,
    ) {
        let method = match session.method {
            crate::delivery::Method::Direct => "direct_play",
            crate::delivery::Method::Remux | crate::delivery::Method::HlsCopy => "remux",
            crate::delivery::Method::Transcode => "transcode",
        };
        let hold_reason = if let Some(reason) = fields.hold_reason {
            Some(reason)
        } else if session.suspended.load(Relaxed) {
            let limits = self.ahead_limits().await;
            let (global_live, global_ahead) = self.global_flow_bytes().await;
            session.ahead().await.and_then(|ahead| {
                ahead_hold(ahead, global_live, global_ahead, limits, true).map(|hold| hold.reason)
            })
        } else {
            None
        }
        .map(|reason| {
            match reason {
                AheadHoldReason::Time => "time",
                AheadHoldReason::Bytes => "bytes",
                AheadHoldReason::Global => "global",
            }
            .to_owned()
        });
        crate::telemetry::emit(
            Arc::clone(&self.store),
            PlaybackEvent {
                at_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
                    .unwrap_or(0),
                session_id: Some(session_id.to_owned()),
                file_id: Some(session.file_id),
                event: event.to_owned(),
                method: Some(method.to_owned()),
                encoder: Some((*session.encoder_label.lock().await).to_owned()),
                height: Some(session.target_height),
                ms: fields.ms,
                speed_recent: session.progress.recent_speed(),
                ahead_seconds: session.ahead().await.map(|ahead| ahead.seconds),
                suspended: Some(session.suspended.load(Relaxed)),
                hold_reason,
                delivered_bps: session.delivery.recent_bps().map(|bytes| bytes * 8),
                readrate: Some(session.readrate),
                reason: fields.reason.map(str::to_owned),
                extra: fields.extra,
                ..PlaybackEvent::default()
            },
        );
    }

    /// End one session now. True if it existed.
    ///
    /// `reason` distinguishes the two callers in the log, because they mean
    /// opposite things: a client releasing a stream it has finished with is
    /// routine, and an admin killing one from the activity page is somebody
    /// intervening.
    pub async fn stop_session(&self, session_id: &str, reason: &'static str) -> bool {
        let Some(session) = self.sessions.lock().await.remove(session_id) else {
            return false;
        };
        session.release_hardware();
        session.release_software();
        session.kill_child().await;
        session.discard_dir().await;
        let event_reason = if reason.contains("released") {
            "client_released"
        } else if reason.contains("admin") {
            "killed"
        } else {
            reason
        };
        self.emit_session_event(
            session_id,
            &session,
            "session_end",
            SessionEventFields {
                reason: Some(event_reason),
                ..SessionEventFields::default()
            },
        )
        .await;
        tracing::info!(%session_id, reason, "transcode session ended");
        true
    }

    async fn touch(&self, session_id: &str, kind: &'static str) -> Option<Arc<Session>> {
        let session = self.sessions.lock().await.get(session_id).cloned()?;
        *session.last_request.lock().await = LastRequest::now(kind);
        Some(session)
    }

    /// Resolve the source and resume base attached to a live HLS capability.
    pub async fn hls_context(&self, session_id: &str) -> Option<HlsContext> {
        let session = self.touch(session_id, "hls-context").await?;
        Some(HlsContext {
            file_id: session.file_id,
            start_seconds: session.start_seconds,
            media_origin_seconds: session.media_origin_seconds,
            codecs: session.hls_codecs.clone(),
            supplemental_codecs: session.hls_supplemental_codecs.clone(),
            frame_rate: None,
        })
    }

    /// Read the current media playlist for a session.
    pub async fn playlist(&self, session_id: &str) -> Option<Vec<u8>> {
        let session = self.touch(session_id, "playlist").await?;
        let path = session.dir.join("index.m3u8");
        // Hold the request until the playlist exists. On the transcode path
        // that is a beat after ffmpeg starts; on the copy path it is the
        // publish gate filling (COPY_PUBLISH_GATE_SECS), which on a
        // NAS-bound 4K remux is production time in the double-digit seconds —
        // so the window is 30 s, not a beat. Holding beats 404ing because the
        // client's patience is asymmetric, verified against the vendored
        // hls.js: a slow first byte is waited on indefinitely
        // (manifestLoadPolicy.maxTimeToFirstByteMs: Infinity, 20 s per
        // attempt, two timeout retries), while a 404 spends one of a single
        // error retry. A failed session still returns None immediately → 404
        // → the player reports an error rather than polling a segment-less
        // playlist on a gray screen forever.
        for _ in 0..PLAYLIST_WAIT_POLLS {
            if session.failed.load(Relaxed) {
                return None;
            }
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if !bytes.is_empty() {
                    // ffmpeg rewrites an EVENT playlist after each segment. Do
                    // not let hls.js race away with the first one-segment
                    // version: its first reload is scheduled at the exact edge
                    // of that segment, which leaves no time for request + append
                    // and creates a deterministic startup stall. This is only a
                    // first-response gate; cached VOD and copy have already
                    // opened it in their constructors, and every reload after
                    // the publication store below stays on the old fast path.
                    if !session.playlist_published.load(Relaxed) {
                        if !transcode_first_playlist_ready(&bytes) {
                            tokio::time::sleep(PLAYLIST_WAIT_POLL).await;
                            continue;
                        }
                        session.playlist_published.store(true, Relaxed);
                    }
                    // The playlist just told us what is published — the one
                    // moment the segment index can be refreshed for free.
                    self.flow_control(&session, session_id).await;
                    if session.cached {
                        return Some(bytes);
                    }
                    let first_retained = session.segments.lock().await.first_retained_index();
                    if let Some(first_retained_index) = first_retained.filter(|index| *index > 0) {
                        if !session.first_slide_logged.swap(true, Relaxed) {
                            let now_unix = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|duration| duration.as_secs() as i64)
                                .unwrap_or(session.started_unix);
                            tracing::info!(
                                session = %session_id,
                                first_retained_index,
                                wall_seconds_since_start =
                                    now_unix.saturating_sub(session.started_unix),
                                "served HLS playlist began sliding"
                            );
                            self.emit_session_event(
                                session_id,
                                &session,
                                "playlist_slide",
                                SessionEventFields {
                                    extra: Some(
                                        serde_json::json!({
                                            "first_retained_index": first_retained_index
                                        })
                                        .to_string(),
                                    ),
                                    ..SessionEventFields::default()
                                },
                            )
                            .await;
                        }
                    }
                    return Some(served_live_playlist(
                        bytes,
                        first_retained,
                        session.typeless_sliding,
                    ));
                }
            }
            tokio::time::sleep(PLAYLIST_WAIT_POLL).await;
        }
        None
    }

    /// Resolve one segment against the complete session timeline.
    ///
    /// The served media playlist becomes a sliding window after retention
    /// starts, but subtitle cues still need the elapsed duration of the
    /// pruned prefix. The index keeps those duration-only entries even after
    /// their files are gone, so callers never reconstruct time from a segment
    /// number or the shortened playlist.
    pub async fn segment_window(&self, session_id: &str, segment_index: i64) -> Option<(f64, f64)> {
        let session = self.touch(session_id, "segment-window").await?;
        self.flow_control(&session, session_id).await;
        let (start_ms, end_ms) = session.segments.lock().await.window_ms_of(segment_index)?;
        Some((start_ms as f64 / 1000.0, end_ms as f64 / 1000.0))
    }

    /// Open a segment for streaming, waiting for ffmpeg to produce it if
    /// necessary.
    ///
    /// Returns an open handle rather than the bytes. One segment of 4K
    /// copy is around 35 MB, and reading that into a `Vec` before Axum sends
    /// its first byte is 35 MB of allocation and a memcpy per request, per
    /// session, four times a minute — for data that is about to be copied
    /// straight back out to a socket. Handing over the open file lets the
    /// response stream, and opening it *here* closes the window where the
    /// retention sweep could unlink the path between resolving it and reading
    /// it: an unlinked file that is already open stays readable.
    pub async fn segment(&self, session_id: &str, name: &str) -> Option<SegmentFile> {
        // Guard against path traversal: segment names are `segNNNNN.ts` only.
        if !is_safe_segment(name) {
            return None;
        }
        let session = self.touch(session_id, "segment").await?;
        let path = session.dir.join(name);
        let idx = segment_index(name);

        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            if let Ok(file) = tokio::fs::File::open(&path).await {
                let len = file.metadata().await.ok()?.len();
                // Counted on open rather than as the body drains: the client
                // has committed to this many bytes, the meter's window is far
                // longer than one fetch takes, and threading a counter through
                // the response stream would buy a precision nobody reads.
                session.delivery.note(len);
                // The client's download frontier just advanced. Resolve it
                // against the index's real EXTINF bounds — the fetched
                // segment's own end time, not an index times a nominal
                // duration — and re-run flow control, since a frontier that
                // moved may have earned the encoder its slot back.
                if let Some(i) = idx {
                    let previous = session.high_segment.fetch_max(i, Relaxed);
                    if i > previous {
                        if let Some(end) = session.segments.lock().await.end_ms_of(i) {
                            session.fetched_end_ms.fetch_max(end, Relaxed);
                        }
                        self.flow_control(&session, session_id).await;
                    }
                }
                return Some(SegmentFile { file, len });
            }
            // Give up if the session was declared dead, or ffmpeg has exited and
            // the file still isn't there.
            if session.failed.load(Relaxed) {
                return None;
            }
            let exited = {
                let mut child = session.child.lock().await;
                child
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
            };
            if exited || Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Delete working directories under the transcode root that no live session
    /// owns. Returns how many were removed.
    ///
    /// The reaper cleans up after sessions it knows about, which covers the
    /// normal case. It cannot cover the abnormal one: a SIGKILL, an OOM, or a
    /// host reboot leaves the directories on disk and the session map empty on
    /// the way back up, so nothing ever claims them. On a 4K library those
    /// leftovers are measured in gigabytes, and the only symptom is a disk
    /// filling for no visible reason.
    pub async fn sweep_orphan_dirs(&self) -> usize {
        let live: std::collections::HashSet<PathBuf> = self
            .sessions
            .lock()
            .await
            .values()
            .map(|s| s.dir.clone())
            .collect();
        // No root yet just means nothing has been transcoded.
        let Ok(mut entries) = tokio::fs::read_dir(&self.work_dir).await else {
            return 0;
        };
        let mut removed = 0usize;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if live.contains(&path) || !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false)
            {
                continue;
            }
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(dir = %path.display(), "removed orphaned transcode directory");
                }
                Err(e) => tracing::warn!(dir = %path.display(), error = %e, "orphan sweep failed"),
            }
        }
        removed
    }

    /// Hold a session that has run far enough ahead of the client, and let it
    /// go again once the client has caught up.
    ///
    /// This is what replaced realtime pacing as the bound on disk. `-re` held
    /// production to exactly the rate of consumption, which bounded the
    /// session directory *and* guaranteed the viewer never had a buffer. The
    /// pair that separates those two concerns is: pace generously (burst, then
    /// a small multiple of realtime) so a buffer actually builds, and stop the
    /// encoder outright once the buffer is deep enough. A stopped ffmpeg costs
    /// nothing and resumes in microseconds — it is the same trick every other
    /// just-in-time server uses, and unlike a rate limit it adapts to a viewer
    /// who pauses.
    ///
    /// Media time resumes 30 seconds below the ceiling (never below half), and
    /// byte limits resume at half. The gap prevents a fast producer from
    /// toggling once per segment near the boundary. This is not a structural
    /// escape for a client that stops fetching: that client may leave the
    /// producer held while already-published media remains available. The
    /// `want_suspend == suspended` guard is load-bearing too: repeated polls
    /// cannot re-signal the child or reset the watchdog's motion clock.
    /// SIGKILL still works on a stopped process, so idle/admin cleanup needs no
    /// special case.
    async fn apply_ahead_window(
        &self,
        session: &Session,
        session_id: &str,
        limits: AheadLimits,
        global_live_bytes: i64,
        global_ahead_bytes: i64,
    ) {
        let Some(ahead) = session.ahead().await else {
            return; // nothing published yet — nothing to hold
        };
        let suspended = session.suspended.load(Relaxed);
        let hold = ahead_hold(
            ahead,
            global_live_bytes,
            global_ahead_bytes,
            limits,
            suspended,
        );
        let want_suspend = hold.is_some();
        if want_suspend == suspended {
            return;
        }
        let signal = if want_suspend {
            libc::SIGSTOP
        } else {
            libc::SIGCONT
        };
        let sent = {
            let child = session.child.lock().await;
            match child.as_ref().and_then(|c| c.id()) {
                // SAFETY: `kill(2)` with a pid this process owns and a signal
                // constant. The child is alive as far as we know; a race with
                // its exit yields ESRCH, which the return check handles.
                Some(pid) => unsafe { libc::kill(pid as libc::pid_t, signal) == 0 },
                None => false, // already reaped
            }
        };
        if !sent {
            return;
        }
        if !want_suspend {
            // Before the flag flips, so the watchdog can never observe
            // "running" beside a motion clock that still spans the suspension
            // — that read would fail a healthy session at the moment of its
            // resume, before ffmpeg has emitted a single post-SIGCONT block.
            session.progress.touch();
        }
        session.suspended.store(want_suspend, Relaxed);
        if want_suspend {
            let hold_reason = hold
                .expect("a requested suspension has a hold reason")
                .reason;
            *session.suspended_at.lock().await = Some((Instant::now(), hold_reason));
            let suspend_count = session.suspend_count.fetch_add(1, Relaxed) + 1;
            tracing::info!(
                session = %session_id,
                suspend_count,
                hold_reason = ?hold.map(|hold| hold.reason),
                release_value = hold.map(|hold| hold.release_value),
                ahead_seconds = ahead.seconds, ahead_bytes = ahead.bytes,
                global_live_bytes, global_ahead_bytes,
                max_secs = limits.max_secs, max_bytes = limits.max_bytes,
                "suspending transcode: far enough ahead of the client"
            );
            self.emit_session_event(
                session_id,
                session,
                "suspend",
                SessionEventFields {
                    hold_reason: Some(hold_reason),
                    ..SessionEventFields::default()
                },
            )
            .await;
        } else {
            let held = session.suspended_at.lock().await.take();
            let held_ms = held.map(|(at, _)| at.elapsed().as_millis().min(i64::MAX as u128) as i64);
            let hold_reason = held.map(|(_, reason)| reason);
            tracing::info!(
                session = %session_id,
                suspend_count = session.suspend_count.load(Relaxed),
                ahead_seconds = ahead.seconds, ahead_bytes = ahead.bytes,
                "resuming transcode: the client caught up"
            );
            self.emit_session_event(
                session_id,
                session,
                "resume",
                SessionEventFields {
                    hold_reason,
                    ms: held_ms,
                    ..SessionEventFields::default()
                },
            )
            .await;
        }
    }

    /// The bounds every session is held to — from the snapshot while it is
    /// fresh, from the settings when it is not.
    async fn ahead_limits(&self) -> AheadLimits {
        if let Some((at, limits)) = *self.cached_limits.read().expect("limits lock") {
            if at.elapsed() < AHEAD_LIMITS_TTL {
                return limits;
            }
        }
        let limits = AheadLimits {
            max_secs: self
                .num_setting(keys::HLS_AHEAD_MAX_SECS, HLS_AHEAD_MAX_SECS_DEFAULT)
                .await,
            max_bytes: self
                .num_setting(keys::HLS_AHEAD_MAX_BYTES, HLS_AHEAD_MAX_BYTES_DEFAULT)
                .await,
            global_max_bytes: self
                .num_setting(keys::HLS_SCRATCH_MAX_BYTES, HLS_SCRATCH_MAX_BYTES_DEFAULT)
                .await,
        };
        // Two refreshers racing both read the same rows; last write wins and
        // they agree to within the TTL anyway.
        *self.cached_limits.write().expect("limits lock") = Some((Instant::now(), limits));
        limits
    }

    /// Forget the snapshot, so the next evaluation reads the settings. For
    /// tests, which assert on the *policy* (cached-until-stale) and must not
    /// spend wall clock waiting a TTL out.
    #[cfg(test)]
    fn forget_cached_limits(&self) {
        *self.cached_limits.write().expect("limits lock") = None;
    }

    /// Both byte views across every live session, from their cached figures —
    /// summing these must not cost a directory walk per session, or the flow
    /// controller could not run on every segment fetch.
    ///
    /// TOTAL bytes enforce the documented disk ceiling
    /// ([`keys::HLS_SCRATCH_MAX_BYTES`]); drainable AHEAD bytes decide when a
    /// global hold may release. The distinction is load-bearing: each
    /// session's retained history is real scratch but cannot fall until its
    /// client frontier moves beyond [`RETENTION_SECS`].
    async fn global_flow_bytes(&self) -> (i64, i64) {
        self.sessions
            .lock()
            .await
            .values()
            .fold((0, 0), |(live, ahead), session| {
                (
                    live + session.live_bytes.load(Relaxed),
                    ahead + session.ahead_bytes.load(Relaxed),
                )
            })
    }

    /// Re-evaluate one session after a client request refreshes the published
    /// index or advances the download frontier.
    ///
    /// The reaper still sweeps every 15 seconds, but as a repair loop. A
    /// window that is only checked on a 15-second tick is not a flow
    /// controller — a fast encoder can put a great deal of 4K on disk between
    /// two ticks.
    async fn flow_control(&self, session: &Session, session_id: &str) {
        session.refresh_segments().await;
        let limits = self.ahead_limits().await;
        let (global_live, global_ahead) = self.global_flow_bytes().await;
        self.apply_ahead_window(session, session_id, limits, global_live, global_ahead)
            .await;
    }

    /// Background loop: kill and remove sessions idle beyond the timeout,
    /// prune played-past segments, and hold sessions that have run ahead.
    pub async fn reap_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(FLOW_CONTROL_REPAIR_INTERVAL);
        loop {
            ticker.tick().await;
            let idle = Duration::from_secs(SESSION_IDLE_SECS);
            let limits = self.ahead_limits().await;
            let mut expired = Vec::new();
            let mut live = Vec::new();
            {
                let sessions = self.sessions.lock().await;
                for (id, s) in sessions.iter() {
                    let last = s.last_request.lock().await;
                    let idle_age = last.at.elapsed();
                    if idle_age > idle {
                        expired.push((id.clone(), Arc::clone(s), idle_age.as_secs(), last.kind));
                    } else {
                        live.push((id.clone(), Arc::clone(s)));
                    }
                }
            }
            for (id, session, idle_seconds, last_request) in expired {
                self.sessions.lock().await.remove(&id);
                session.release_hardware();
                session.release_software();
                // Kills a suspended child too — SIGKILL is not blockable and
                // does not need the process scheduled to take effect.
                session.kill_child().await;
                session.discard_dir().await;
                let end_reason = if session.failed.load(Relaxed) {
                    "failed"
                } else {
                    "idle"
                };
                self.emit_session_event(
                    &id,
                    &session,
                    "session_end",
                    SessionEventFields {
                        reason: Some(end_reason),
                        ..SessionEventFields::default()
                    },
                )
                .await;
                tracing::info!(
                    session_id = %id,
                    idle_seconds,
                    last_request,
                    "reaped idle transcode session"
                );
            }
            // What this box actually achieves, remembered per class of work.
            // Admission asks it the next time hardware is full, so the answer
            // to "can software cope with this" is a measurement from this
            // machine rather than an assumption about machines in general.
            // A suspended session is making no progress on purpose and would
            // poison the record with a speed it was never asked to reach.
            for (_id, session) in &live {
                let class = session.class.lock().expect("class mutex").clone();
                if class.is_empty() || session.suspended.load(Relaxed) {
                    continue;
                }
                if let Some(speed) = session.progress.recent_speed() {
                    self.admissions.record(&class, speed);
                }
            }
            // Repair pass. Client requests run flow control on playlist/index
            // refresh and frontier advance; this catches a producer that no
            // client is currently requesting from, and prunes retention.
            for (_id, session) in &live {
                session.refresh_segments().await;
                gc_expired_segments(session).await;
            }
            let (global_live, global_ahead) = self.global_flow_bytes().await;
            for (id, session) in &live {
                self.apply_ahead_window(session, id, limits, global_live, global_ahead)
                    .await;
            }
        }
    }
}

/// Only `segNNNNN.ts` names are valid segment requests.
fn is_safe_segment(name: &str) -> bool {
    // fMP4 (copy-video) HLS: a single shared init segment.
    if name == "init.mp4" {
        return true;
    }
    // `segNNNNN.ts` (transcode) or `segNNNNN.m4s` (copy fMP4).
    name.strip_prefix("seg")
        .and_then(|rest| {
            rest.strip_suffix(".ts")
                .or_else(|| rest.strip_suffix(".m4s"))
        })
        .map(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// The numeric index of a segment filename (`segNNNNN.ts`/`.m4s`), or None for
/// the init segment, the playlist, or anything else.
fn segment_index(name: &str) -> Option<i64> {
    name.strip_prefix("seg")
        .and_then(|rest| {
            rest.strip_suffix(".ts")
                .or_else(|| rest.strip_suffix(".m4s"))
        })
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<i64>().ok())
}

/// Delete published segments that have fallen out of the retention window.
///
/// The window is measured back from the client's DOWNLOAD frontier, not from
/// its playhead, and is wide enough to cover the difference (see
/// [`RETENTION_SECS`]). The previous version counted a fixed number of
/// segments back from the furthest one fetched, which was two mistakes at
/// once: a segment count is not a duration on the copy path, and the frontier
/// is not where the viewer is. On the physical iPad that reproduced this,
/// AVPlayer fetched about 120 seconds ahead despite a 60-second preference;
/// retaining only 120 seconds therefore moved the playlist start onto the
/// playhead and left no reload margin.
///
/// `init.mp4` and the playlist are never candidates — neither carries an
/// EXTINF, so neither appears in the index.
async fn gc_expired_segments(session: &Session) {
    // Never against a cache entry. Retention exists to bound the scratch a
    // live encoder is producing; a cached asset is a finished artifact that
    // other viewers will want whole. Pruning one would eat it from the front
    // as somebody watched — and leave the row still saying `complete`, so the
    // next viewer gets a playlist whose opening segments 404.
    if session.cached {
        return;
    }
    let frontier = session.fetched_end_ms.load(Relaxed);
    let keep_from = frontier - RETENTION_SECS * 1000;
    if keep_from <= 0 {
        return; // nothing can be old enough yet
    }
    let doomed: Vec<String> = {
        let index = session.segments.lock().await;
        index.prunable(keep_from).map(|s| s.name.clone()).collect()
    };
    if doomed.is_empty() {
        return;
    }
    for name in &doomed {
        let _ = tokio::fs::remove_file(session.dir.join(name)).await;
    }
    // Forget their sizes so the budget stops counting bytes that are no longer
    // on disk.
    let doomed: std::collections::HashSet<&str> = doomed.iter().map(String::as_str).collect();
    let mut index = session.segments.lock().await;
    for seg in index.segs.iter_mut() {
        if doomed.contains(seg.name.as_str()) {
            seg.bytes = 0;
            // The file is gone; without the flag every later refresh would
            // re-stat it forever to relearn that.
            seg.pruned = true;
        }
    }
    // The budget number follows the deletion now, not at the next refresh:
    // freed disk a suspended session cannot trigger a refresh for should
    // release the global cap on the reaper pass that freed it.
    session.live_bytes.store(index.total_bytes(), Relaxed);
}

/// A sensible video bitrate (kbps) for a target height.
fn bitrate_for_height(height: i64) -> u32 {
    match height {
        h if h >= 2160 => 20_000,
        h if h >= 1080 => 8_000,
        h if h >= 720 => 4_000,
        h if h >= 480 => 2_000,
        _ => 1_200,
    }
}

/// The quality ladder's rungs, bottom to top (ADAPTIVE-QUALITY.md).
///
/// 4K output rungs are deliberately absent: a client that can take 20 Mb/s
/// sustained is better served by direct play or remux, and a 4K→4K
/// transcode burns GPU for nothing. Heights ABOVE the ladder still exist as
/// explicit requests — Original with a forced burn carries the source's own
/// height, a promise nothing here may downgrade.
pub const LADDER_HEIGHTS: [i64; 4] = [360, 480, 720, 1080];

/// One advertised rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Rung {
    pub height: i64,
    /// The rung's nominal cost on the wire: video target + audio, in kb/s.
    /// This is the number an adaptation controller compares its estimate to
    /// (its thresholds carry their own headroom factors).
    pub total_kbps: u32,
    /// What the rung may PEAK at over the rate-control window: `-maxrate`
    /// (1.5× the target, PERF-PLAN §4.6) + audio. The number that must
    /// cover the measured burst — nynuc measured 9.05 Mb/s on the 1080
    /// rung's 8 Mb/s target — and the one an HLS `BANDWIDTH` attribute
    /// would be required to state.
    pub peak_kbps: u32,
}

/// The ladder as advertised to a client, top rung first, filtered to what
/// the source can actually feed: rungs above the source height are dropped
/// (a 720p file offers 720p and below — never an upscale), and an unprobed
/// source offers the whole ladder because there is nothing to filter by.
pub fn ladder(source_height: Option<i64>) -> Vec<Rung> {
    LADDER_HEIGHTS
        .iter()
        .rev()
        .filter(|h| source_height.filter(|s| *s > 0).is_none_or(|s| **h <= s))
        .map(|&height| {
            let video = bitrate_for_height(height);
            Rung {
                height,
                total_kbps: video + plurx_core::transcode::AUDIO_BITRATE_KBPS_DEFAULT,
                peak_kbps: video * 3 / 2 + plurx_core::transcode::AUDIO_BITRATE_KBPS_DEFAULT,
            }
        })
        .collect()
}

/// Snap an explicitly requested height onto the ladder: nearest rung, ties
/// DOWN (bandwidth is the scarce thing). Two escapes, both deliberate.
/// Heights above the top rung pass through untouched — they are
/// Original-class requests (a forced-subtitle burn under Original carries a
/// 4K source's own 2160), not strays. And the caller must not snap a
/// request for the source's own height for the same reason; that exception
/// needs the file and so lives at the call site.
pub fn snap_height(height: i64) -> i64 {
    let top = LADDER_HEIGHTS[LADDER_HEIGHTS.len() - 1];
    if height > top {
        return height;
    }
    LADDER_HEIGHTS
        .iter()
        .copied()
        .min_by_key(|rung| ((rung - height).abs(), *rung))
        .unwrap_or(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_gets_the_app_owned_runtime_cache() {
        use plurx_core::store::SqliteStore;

        let root = tempfile::tempdir().expect("root");
        let work = root.path().join("transcode");
        let finished = root.path().join("cache").join("transcode");
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let manager = TranscodeManager::new(store, work, EncoderCaps::default(), Pipeline::Cpu)
            .with_cache(finished, "test-ffmpeg".into(), "test-node".into());

        let expected = root.path().join("cache").join("runtime");
        assert_eq!(manager.runtime_cache, expected);
        assert!(expected.is_dir(), "the cache exists before ffmpeg starts");

        let mut command = tokio::process::Command::new("ffmpeg");
        configure_ffmpeg_runtime(&mut command, &manager.runtime_cache);
        let inherited = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "XDG_CACHE_HOME")
            .and_then(|(_, value)| value)
            .expect("XDG_CACHE_HOME");
        assert_eq!(inherited, expected.as_os_str());
    }

    #[test]
    fn safe_segment_names() {
        assert!(is_safe_segment("seg00000.ts"));
        assert!(is_safe_segment("seg12345.ts"));
        assert!(is_safe_segment("seg00000.m4s")); // copy fMP4 segment
        assert!(is_safe_segment("init.mp4")); // copy fMP4 init
        assert!(!is_safe_segment("seg.ts"));
        assert!(!is_safe_segment("seg.m4s"));
        assert!(!is_safe_segment("../seg00000.ts"));
        assert!(!is_safe_segment("index.m3u8"));
        assert!(!is_safe_segment("other.mp4"));
        assert!(!is_safe_segment("seg0/../../etc.ts"));
    }

    #[test]
    fn bitrate_ladder() {
        assert_eq!(bitrate_for_height(2160), 20_000);
        assert_eq!(bitrate_for_height(1080), 8_000);
        assert_eq!(bitrate_for_height(720), 4_000);
        assert_eq!(bitrate_for_height(240), 1_200);
    }

    #[test]
    fn durable_quality_distinguishes_unset_default_from_corruption() {
        assert_eq!(
            normalize_rate_control_request(Some("quality"), None),
            (RateMode::Quality, None, false)
        );
        assert_eq!(
            normalize_rate_control_request(Some("quality"), Some("  ")),
            (RateMode::Quality, None, false)
        );
        assert_eq!(
            normalize_rate_control_request(Some("quality"), Some("22")),
            (RateMode::Quality, Some(22), false)
        );
        for corrupt in ["256", "garbage", "-1"] {
            assert_eq!(
                normalize_rate_control_request(Some("quality"), Some(corrupt)),
                (RateMode::Bitrate, None, true),
                "{corrupt} must fail the pair closed rather than aliasing the family default"
            );
        }
        assert_eq!(
            normalize_rate_control_request(Some("cq"), Some("22")),
            (RateMode::Bitrate, None, true)
        );
    }

    #[tokio::test]
    async fn boot_fails_a_corrupt_quality_pair_closed_to_legacy_vbr() {
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        store
            .put_settings(&[
                (keys::TRANSCODE_RATE_MODE, "quality"),
                (keys::TRANSCODE_QUALITY, "256"),
            ])
            .await
            .expect("corrupt durable pair");
        let (mgr, _work, _cache) = cached_manager(&store);

        mgr.initialize_rate_control().await.expect("boot fallback");
        assert_eq!(
            mgr.rate_control_snapshot().requested_mode,
            RateMode::Bitrate
        );
        assert_eq!(
            mgr.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Vbr
        );
    }

    #[test]
    fn requested_quality_resolves_per_family_and_fails_closed() {
        let mut supported = QualityRc::default();
        supported.set_supported(Encoder::Qsv, true);
        let snapshot = RateControlSnapshot {
            requested_mode: RateMode::Quality,
            requested_quality: Some(21),
            quality_rc: supported,
        };
        assert_eq!(
            snapshot.effective_for(Encoder::Qsv),
            EffectiveRateControl::Qvbr { quality: 21 }
        );
        assert_eq!(
            snapshot.effective_for(Encoder::Software),
            EffectiveRateControl::Vbr,
            "a requested value cannot outrun this family's probe verdict"
        );

        let defaults = RateControlSnapshot {
            requested_mode: RateMode::Quality,
            requested_quality: None,
            quality_rc: supported,
        };
        assert_eq!(
            defaults.effective_for(Encoder::Qsv),
            EffectiveRateControl::Qvbr {
                quality: Encoder::Qsv.default_quality()
            }
        );

        let mut cross_family = supported;
        cross_family.set_supported(Encoder::Software, true);
        cross_family.set_supported(Encoder::VideoToolbox, true);
        let defaults = RateControlSnapshot {
            requested_mode: RateMode::Quality,
            requested_quality: None,
            quality_rc: cross_family,
        };
        assert_eq!(
            defaults.effective_for(Encoder::VideoToolbox),
            EffectiveRateControl::Qvbr { quality: 65 }
        );
        assert_eq!(
            defaults.effective_for(Encoder::Software),
            EffectiveRateControl::Qvbr { quality: 23 },
            "a runtime hardware fallback must re-resolve the destination family's default"
        );
        assert_eq!(
            RateControlSnapshot::bitrate(supported).effective_for(Encoder::Qsv),
            EffectiveRateControl::Vbr
        );
    }

    #[tokio::test]
    async fn offline_identity_stays_vbr_until_its_snapshot_is_durable() {
        use plurx_core::domain::{NewOfflinePackage, OfflineCreateOutcome};
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store.create_user("paul", "hash", true).await.expect("user");
        let file_id = seed_file(&store).await;
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let (mgr, _work, _cache) = cached_manager(&store);
        let mut supported = QualityRc::default();
        supported.set_supported(Encoder::Software, true);
        let package_id = "offline-vbr-snapshot";
        let requested = NewOfflinePackage {
            id: package_id.to_owned(),
            request_id: "offline-vbr-snapshot-request".to_owned(),
            user_id: user.id,
            file_id,
            node_id: NODE.to_owned(),
            source_path: file.path.to_string_lossy().into_owned(),
            source_size: file.size,
            source_mtime: file.mtime,
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: None,
            audio_offset_ms: 0,
            subtitle_index: None,
            subtitle_language: None,
            subtitle_mode: "none".to_owned(),
            estimated_bytes: 1_000_000,
            reserved_bytes: 1_100_000,
            expires_at: i64::MAX,
        };
        assert!(matches!(
            store
                .create_offline_package(&requested, 10, 10_000_000, 20_000_000)
                .await
                .expect("create package"),
            OfflineCreateOutcome::Created(_)
        ));
        store
            .claim_next_offline_package(NODE)
            .await
            .expect("claim")
            .expect("queued package");

        let set_quality = |quality| {
            *mgr.rate_control
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = RateControlSnapshot {
                requested_mode: RateMode::Quality,
                requested_quality: Some(quality),
                quality_rc: supported,
            };
        };
        let spec = OfflineSpec {
            target_height: 720,
            audio_index: None,
            subtitle: OfflineSubtitle::None,
        };
        set_quality(21);
        assert!(matches!(
            mgr.ensure_offline(
                package_id,
                &file,
                &spec,
                Instant::now(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("first offline pass"),
            OfflineProduceOutcome::Yielded
        ));
        let first_hash = store
            .offline_package_for_user(package_id, user.id)
            .await
            .expect("read package")
            .expect("package")
            .recipe_hash
            .expect("pinned recipe");

        assert!(store
            .requeue_offline_package(package_id)
            .await
            .expect("requeue"));
        store
            .claim_next_offline_package(NODE)
            .await
            .expect("claim again")
            .expect("queued package");
        set_quality(29);
        assert!(matches!(
            mgr.ensure_offline(
                package_id,
                &file,
                &spec,
                Instant::now(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("resumed offline pass after hot setting change"),
            OfflineProduceOutcome::Yielded
        ));
        assert_eq!(
            store
                .offline_package_for_user(package_id, user.id)
                .await
                .expect("read resumed package")
                .expect("package")
                .recipe_hash
                .as_deref(),
            Some(first_hash.as_str()),
            "ensure_offline must keep the package's legacy identity across a hot quality change"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_refresh_loop_updates_the_hot_path_snapshot() {
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let (writer, _writer_work, _writer_cache) = cached_manager(&store);
        let (peer, _peer_work, _peer_cache) = cached_manager(&store);
        let peer = Arc::new(peer);
        let mut supported = QualityRc::default();
        supported.set_supported(Encoder::Software, true);
        peer.publish_rate_control(
            RateControlSnapshot {
                requested_mode: RateMode::Quality,
                requested_quality: Some(21),
                quality_rc: supported,
            },
            Encoder::Software,
        );
        assert_eq!(
            peer.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Qvbr { quality: 21 }
        );

        writer
            .apply_rate_control_settings(RateMode::Bitrate, None)
            .await
            .expect("replicated write");
        let refresh = tokio::spawn(Arc::clone(&peer).rate_control_refresh_loop());
        tokio::task::yield_now().await;
        tokio::time::advance(RATE_CONTROL_REFRESH).await;
        for _ in 0..100 {
            if peer.effective_rate_control(Encoder::Software) == EffectiveRateControl::Vbr {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            peer.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Vbr,
            "the two-second refresher must replace the peer's stale snapshot"
        );
        refresh.abort();
    }

    #[tokio::test]
    async fn a_peer_refresh_defers_quality_validation_while_a_viewer_waits() {
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let (peer, _work, _cache) = cached_manager(&store);
        store
            .put_settings(&[
                (keys::TRANSCODE_RATE_MODE, "quality"),
                (keys::TRANSCODE_QUALITY, "22"),
            ])
            .await
            .expect("replicated quality request");
        let _viewer = peer.admissions.wait_for_slot();

        assert!(
            peer.refresh_rate_control()
                .await
                .expect("refresh")
                .is_none(),
            "the runtime probe must be deferred, not treated as a driver refusal"
        );
        assert_eq!(
            peer.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Vbr,
            "the last validated snapshot remains published while the viewer has priority"
        );
    }

    #[tokio::test]
    async fn an_admin_quality_change_does_not_mutate_settings_while_a_viewer_waits() {
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let (mgr, _work, _cache) = cached_manager(&store);
        let _viewer = mgr.admissions.wait_for_slot();

        assert!(matches!(
            mgr.apply_rate_control_settings(RateMode::Quality, Some(22))
                .await,
            Err(ApplyRateControlError::Busy)
        ));
        assert_eq!(
            mgr.requested_rate_control().await.expect("requested pair"),
            (RateMode::Bitrate, None),
            "a deferred validation must not durably publish an unvalidated request"
        );
        assert_eq!(
            mgr.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Vbr
        );
    }

    #[tokio::test]
    async fn runtime_quality_validation_never_overlaps_the_background_encoder_lane() {
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let (mgr, _work, _cache) = cached_manager(&store);
        let _producer = mgr.background_producer.lock().await;

        assert!(matches!(
            mgr.apply_rate_control_settings(RateMode::Quality, Some(22))
                .await,
            Err(ApplyRateControlError::Busy)
        ));
        assert_eq!(
            mgr.requested_rate_control().await.expect("requested pair"),
            (RateMode::Bitrate, None),
            "validation must defer before writing while offline/speculative encoding owns the lane"
        );
    }

    #[tokio::test]
    async fn a_fallback_uses_the_session_generation_not_the_latest_setting() {
        use plurx_core::store::SqliteStore;

        super::require_ffmpeg();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let (mgr, _work, _cache) = cached_manager(&store);
        let mut supported = QualityRc::default();
        supported.set_supported(Encoder::VideoToolbox, true);
        supported.set_supported(Encoder::Software, true);
        let captured = RateControlSnapshot {
            requested_mode: RateMode::Quality,
            requested_quality: None,
            quality_rc: supported,
        };
        let later = RateControlSnapshot {
            requested_mode: RateMode::Quality,
            requested_quality: Some(31),
            quality_rc: supported,
        };
        *mgr.rate_control
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = later;

        let dir = tempfile::tempdir().expect("session dir");
        let session = test_session(dir.path().to_path_buf());
        let mut opts = mgr.options_for_tone_map(
            Encoder::VideoToolbox,
            &file,
            720,
            0.0,
            None,
            None,
            None,
            ToneMap::Zscale,
        );
        opts.pipeline = Pipeline::Cpu;
        opts.effective_rate_control = captured.effective_for(Encoder::VideoToolbox);
        let sw_pool = mgr.admissions.software_pool();
        let fallback = TranscodeManager::downgrade_one_step(
            &session,
            &file,
            &opts,
            Encoder::VideoToolbox,
            captured.effective_for(Encoder::Software),
            Pacing::unpaced(),
            &sw_pool,
            dir.path(),
            "generation-test",
            &mgr.runtime_cache,
        )
        .await;
        session.kill_child().await;

        assert_eq!(
            fallback,
            EffectiveRateControl::Qvbr { quality: 23 },
            "the production downgrade seam must use the session's captured generation"
        );
        assert_ne!(
            fallback,
            mgr.effective_rate_control(Encoder::Software),
            "the global q31 setting is reserved for a later session"
        );
    }

    /// The snap normalises menu strays onto the ladder — nearest rung, ties
    /// DOWN, because bandwidth is the scarce thing — and refuses to touch
    /// what is not a stray: anything above the ladder is an Original-class
    /// request carrying a source's own height.
    #[test]
    fn stray_heights_snap_to_the_ladder_and_promises_pass_through() {
        assert_eq!(snap_height(1080), 1080, "a rung is already home");
        assert_eq!(snap_height(540), 480, "nearest");
        assert_eq!(snap_height(600), 480, "equidistant resolves DOWN");
        assert_eq!(snap_height(900), 720, "equidistant resolves DOWN");
        assert_eq!(
            snap_height(144),
            360,
            "below the ladder climbs to its floor"
        );
        assert_eq!(
            snap_height(1440),
            1440,
            "above the ladder is a promise, not a stray"
        );
        assert_eq!(snap_height(2160), 2160);
    }

    /// The advertised ladder follows the source down and never offers an
    /// upscale; its totals are the wire cost the controller reasons about,
    /// and its peaks are what the rate-control window may actually spend.
    #[test]
    fn the_ladder_is_filtered_by_the_source_and_priced_both_ways() {
        let full = ladder(Some(2160));
        assert_eq!(
            full.iter().map(|r| r.height).collect::<Vec<_>>(),
            vec![1080, 720, 480, 360],
            "top first, no 4K output rung — that is what remux is for"
        );
        let top = full[0];
        assert_eq!(top.total_kbps, 8_160, "8 Mb/s video + 160 kb/s audio");
        assert_eq!(top.peak_kbps, 12_160, "the -maxrate window bound + audio");

        assert_eq!(
            ladder(Some(720))
                .iter()
                .map(|r| r.height)
                .collect::<Vec<_>>(),
            vec![720, 480, 360],
            "a 720p file offers 720p and below — never an upscale"
        );
        assert!(
            ladder(Some(240)).is_empty(),
            "nothing to offer below the floor"
        );
        assert_eq!(
            ladder(None).len(),
            4,
            "an unprobed source has nothing to filter by"
        );
        assert_eq!(ladder(Some(0)).len(), 4, "0 is not a height");
    }

    /// Auto is a policy about the encoder, not about the file.
    #[tokio::test]
    async fn auto_follows_the_source_only_when_hardware_can_carry_it() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let work = tempfile::tempdir().expect("work");
        let hardware = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps {
                nvenc: true,
                ..Default::default()
            },
            Pipeline::Cpu,
        );
        let software = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );

        // Software keeps the conservative ceiling whatever the source is: a
        // 1080p x264 session on a NUC cannot hold realtime, and a stream that
        // stutters at 1080p is worse than one that plays at 720p.
        for source in [Some(2160), Some(1080), None] {
            assert_eq!(software.auto_height(source).await, 720, "source {source:?}");
        }
        // …and never advertises a rung above the source: the filter already
        // refused to upscale, but the bitrate and the response metadata
        // described 720p for a 480p stream (§3.1).
        assert_eq!(software.auto_height(Some(480)).await, 480, "never upscales");

        // Hardware follows the source, capped at 1080 — 4K is a bandwidth
        // decision as well as a CPU one, and Auto should not put 20 Mb/s on
        // somebody's Wi-Fi without being asked.
        assert_eq!(hardware.auto_height(Some(2160)).await, 1080, "4K is capped");
        assert_eq!(hardware.auto_height(Some(1080)).await, 1080);
        assert_eq!(hardware.auto_height(Some(480)).await, 480, "never upscales");
        // An unprobed file has no height to follow; 1080 is the useful guess,
        // and the encoder being asked is the one that can carry it.
        assert_eq!(hardware.auto_height(None).await, 1080);
        assert_eq!(
            hardware.auto_height(Some(0)).await,
            1080,
            "0 is not a height"
        );
    }

    /// The progress stream is the only thing that can tell slow from stuck, so
    /// what it refuses to believe matters as much as what it parses.
    #[test]
    fn progress_lines_parse_and_reject() {
        let p = Progress::new();
        let gen = p.begin_attempt();
        assert_eq!(
            p.out_time_ms(),
            None,
            "nothing known before the first block"
        );
        assert_eq!(p.speed(), None);

        // `N/A` arrives before the first frame lands, and for `speed` on a
        // session too young to estimate one. It is not zero: zero out_time
        // reads as "produced nothing" and zero speed reads as "stalled".
        for line in ["out_time_us=N/A", "speed=N/A", "bitrate=N/A", "frame=0"] {
            apply_progress_line(&p, gen, line);
        }
        assert_eq!(p.out_time_ms(), None);
        assert_eq!(p.speed(), None);

        // Microseconds in, milliseconds out — `out_time_ms` is misnamed by
        // ffmpeg and also carries microseconds, so both spellings mean the same.
        apply_progress_line(&p, gen, "out_time_us=5960000");
        assert_eq!(p.out_time_ms(), Some(5960));
        apply_progress_line(&p, gen, "out_time_ms=8000000");
        assert_eq!(p.out_time_ms(), Some(8000));

        apply_progress_line(&p, gen, "speed=46.2x");
        assert_eq!(p.speed(), Some(46.2));
        apply_progress_line(&p, gen, "speed=0.71x");
        assert_eq!(p.speed(), Some(0.71));

        // Junk and unknown keys change nothing.
        for line in ["", "no-equals-sign", "speed=", "out_time_us=abc", "fps=25"] {
            apply_progress_line(&p, gen, line);
        }
        assert_eq!(p.out_time_ms(), Some(8000));
        assert_eq!(p.speed(), Some(0.71));

        // A respawn (the hardware→software fallback) starts a fresh timeline;
        // carrying the dead process's numbers would read as an instant stall.
        p.begin_attempt();
        assert_eq!(p.out_time_ms(), None);
        assert_eq!(p.speed(), None);
    }

    /// Staleness must measure the last time output *moved*, not the last block
    /// received: a wedged ffmpeg keeps emitting progress with a frozen time.
    #[test]
    fn stall_is_measured_from_movement_not_from_chatter() {
        let p = Progress::new();
        let gen = p.begin_attempt();
        apply_progress_line(&p, gen, "out_time_us=1000000");
        let after_move = p.stalled_for();
        // Re-reporting the same timestamp is not progress.
        for _ in 0..5 {
            apply_progress_line(&p, gen, "out_time_us=1000000");
            apply_progress_line(&p, gen, "speed=0.9x");
        }
        assert!(
            p.stalled_for() >= after_move,
            "repeating a timestamp must not reset the stall clock"
        );
        // Moving forward does reset it.
        std::thread::sleep(Duration::from_millis(5));
        let before = p.stalled_for();
        apply_progress_line(&p, gen, "out_time_us=2000000");
        assert!(p.stalled_for() <= before);
    }

    /// The race this closes: a killed attempt's stdout reader is still holding
    /// buffered lines when the replacement starts, and those lines describe a
    /// process that got further along. Applied to the new attempt they read as
    /// "produced a lot, then froze" -- a healthy encoder declared dead by its
    /// predecessor.
    #[test]
    fn a_dead_attempt_cannot_speak_for_its_replacement() {
        let p = Progress::new();
        let first = p.begin_attempt();
        apply_progress_line(&p, first, "out_time_us=30000000");
        assert_eq!(p.out_time_ms(), Some(30_000));

        // The fallback fires: new attempt, clean slate.
        let second = p.begin_attempt();
        assert_ne!(first, second);
        assert_eq!(p.out_time_ms(), None, "the replacement starts unmeasured");

        // The dead reader drains its buffer. None of it lands.
        for line in ["out_time_us=30000000", "speed=4.0x", "out_time_us=31000000"] {
            apply_progress_line(&p, first, line);
        }
        assert_eq!(p.out_time_ms(), None, "stale attempt was ignored");
        assert_eq!(p.speed(), None);

        // The live attempt is believed.
        apply_progress_line(&p, second, "out_time_us=2000000");
        assert_eq!(p.out_time_ms(), Some(2_000));
    }

    /// Cumulative speed hides a slowdown behind a fast start; the recent rate
    /// is what predicts whether the viewer's reserve is about to drain. The
    /// smoothing is tested as arithmetic — a test that had to sleep to make a
    /// sample could not cover the rejection rules at all.
    #[test]
    fn recent_rate_smooths_and_rejects() {
        // First usable sample seeds the average: 2s of content in 1s = 2.00x.
        assert_eq!(recent_rate_step(-1, 1_000, 2_000), Some(2_000));
        // Subsequent samples bend it rather than replacing it: a crawl after a
        // fast start pulls down, but not all the way in one step.
        let bent = recent_rate_step(2_000, 1_000, 100).expect("a rate");
        assert!(
            bent < 2_000 && bent > 100,
            "smoothed toward the new rate: {bent}"
        );
        // Sustained crawling converges on the truth.
        let mut r = 2_000;
        for _ in 0..40 {
            r = recent_rate_step(r, 1_000, 100).expect("a rate");
        }
        assert!(r < 200, "converged on the real rate: {r}");

        // A gap longer than the cutoff spans a suspend — it must not report
        // the stopped time as slowness.
        assert_eq!(
            recent_rate_step(2_000, RECENT_SAMPLE_MAX_GAP_MS + 1, 500),
            None
        );
        // Two adjacent ffmpeg blocks are jitter, not signal.
        assert_eq!(recent_rate_step(2_000, RECENT_SAMPLE_MIN_MS - 1, 400), None);
        // Output going backwards is nonsense, not a negative rate.
        assert_eq!(recent_rate_step(2_000, 1_000, -50), None);
    }

    /// End to end through the atomics: the rate appears, and a gapped sample
    /// leaves it alone.
    #[test]
    fn recent_speed_survives_a_hold() {
        let p = Progress::new();
        let generation = p.begin_attempt();
        assert_eq!(p.recent_speed(), None, "nothing to average yet");
        // Baseline, then a sample far enough apart in wall clock to count.
        // (The test's own clock barely advances, so the baseline is backdated
        // rather than slept for.)
        apply_progress_line(&p, generation, "out_time_us=0");
        p.sample_wall_ms.store(-1_000, Relaxed);
        p.sample_out_ms.store(0, Relaxed);
        apply_progress_line(&p, generation, "out_time_us=4000000");
        let seen = p.recent_speed().expect("a rate once two samples exist");
        assert!(seen > 0.0);

        // Now a sample that looks like it spans a long hold: unchanged.
        p.sample_wall_ms
            .store(-RECENT_SAMPLE_MAX_GAP_MS * 2, Relaxed);
        apply_progress_line(&p, generation, "out_time_us=5000000");
        assert_eq!(
            p.recent_speed(),
            Some(seen),
            "the hold did not count as slow"
        );
    }

    /// Watching a stream is not fetching from it.
    ///
    /// The stats overlay polls session status every couple of seconds while it
    /// is open. If that poll touched the idle clock, leaving the overlay up
    /// would keep an abandoned encoder alive forever — the exact leak the idle
    /// reaper exists to prevent. This is already true; the test is here so it
    /// stays true, because the natural way to write `session_status` is to
    /// reuse `touch` and nothing would visibly break.
    #[tokio::test]
    async fn polling_status_does_not_keep_a_session_alive() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        let info = mgr
            .start(file_id, 720, 0.0, None, None, "paul", "pb-paul")
            .await
            .expect("start");

        // Let the idle clock advance, then poll status several times.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let before = mgr.session_status(&info.session_id).await.expect("status");
        assert_eq!(before.hold_reason, None, "a running session is not held");
        assert_eq!(before.resume_below_seconds, None);
        assert_eq!(before.resume_below_bytes, None);
        let before = before.idle_seconds;
        for _ in 0..5 {
            assert!(mgr.session_status(&info.session_id).await.is_some());
        }
        let after = mgr
            .session_status(&info.session_id)
            .await
            .expect("status")
            .idle_seconds;
        assert!(
            after >= before,
            "idle time must keep running while status is polled"
        );

        // A real fetch DOES reset it — the contrast that keeps the assertion
        // above from passing vacuously on a session whose clock never moved.
        // (`touch` is the shared front half of the playlist and segment
        // readers; calling those here would long-poll for output this fixture
        // can never produce.)
        assert!(mgr.touch(&info.session_id, "test-fetch").await.is_some());
        assert_eq!(
            mgr.session_status(&info.session_id)
                .await
                .expect("status")
                .idle_seconds,
            0,
            "fetching from a session is what keeps it alive"
        );
        assert!(mgr.stop_session(&info.session_id, "test").await);
    }

    /// The playlist is the only place a copied segment's true duration is
    /// written down, which is the whole reason this parser exists.
    #[test]
    fn playlist_parsing_believes_extinf_not_the_index() {
        // A copy session: `hls_time` is only a floor there, and real segments
        // run to the source's GOP. Index arithmetic against a nominal 4 s
        // would call the third segment 8-12s; it is actually 14.5-24.5s.
        let copy = "#EXTM3U\n\
                    #EXT-X-VERSION:7\n\
                    #EXT-X-TARGETDURATION:11\n\
                    #EXT-X-PLAYLIST-TYPE:EVENT\n\
                    #EXT-X-MAP:URI=\"init.mp4\"\n\
                    #EXTINF:4.000,\n\
                    seg00000.m4s\n\
                    #EXTINF:10.500,\n\
                    seg00001.m4s\n\
                    #EXTINF:10.000,\n\
                    seg00002.m4s\n";
        let segs = parse_playlist(copy);
        assert_eq!(segs.len(), 3, "the EXT-X-MAP init file is not a segment");
        assert_eq!(segs[0].name, "seg00000.m4s");
        assert_eq!((segs[0].start_ms, segs[0].end_ms), (0, 4_000));
        assert_eq!((segs[1].start_ms, segs[1].end_ms), (4_000, 14_500));
        assert_eq!((segs[2].start_ms, segs[2].end_ms), (14_500, 24_500));

        let index = SegmentIndex { segs };
        assert_eq!(index.produced_playable_end_ms(), Some(24_500));
        assert_eq!(index.end_ms_of(1), Some(14_500));
        assert_eq!(index.window_ms_of(1), Some((4_000, 14_500)));
        assert_eq!(index.end_ms_of(9), None);

        // A transcode playlist, where the grid is forced and even.
        let transcode = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
                         #EXTINF:4.000000,\n seg00000.ts\n\
                         #EXTINF:4.000000,\nseg00001.ts\n";
        let segs = parse_playlist(transcode);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].end_ms, 8_000);

        // Junk: a URI with no EXTINF, an EXTINF with no URI, an unparseable
        // duration. None of them may invent a segment or shift the timeline.
        let junk = "#EXTM3U\nseg00007.ts\n#EXTINF:abc,\nseg00008.ts\n#EXTINF:2.0,\n";
        assert!(parse_playlist(junk).is_empty());
        assert!(parse_playlist("").is_empty());
    }

    /// A live EVENT playlist needs both more than one segment and enough media
    /// runway before its first response. A long first segment alone still
    /// leaves hls.js at the writer edge, while a completed short title must not
    /// be held until the request timeout.
    #[test]
    fn live_transcode_publication_requires_real_cushion_or_endlist() {
        let one_short = b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                          #EXTINF:2.000,\nseg00000.ts\n";
        let one_long = b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                         #EXTINF:8.000,\nseg00000.ts\n";
        let two_short = b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                          #EXTINF:1.000,\nseg00000.ts\n\
                          #EXTINF:1.000,\nseg00001.ts\n";
        let two_ready = b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                          #EXTINF:2.000,\nseg00000.ts\n\
                          #EXTINF:2.000,\nseg00001.ts\n";
        let completed_short = b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                                #EXTINF:1.000,\nseg00000.ts\n#EXT-X-ENDLIST\n";

        assert!(!transcode_first_playlist_ready(one_short));
        assert!(!transcode_first_playlist_ready(one_long));
        assert!(!transcode_first_playlist_ready(two_short));
        assert!(transcode_first_playlist_ready(two_ready));
        assert!(transcode_first_playlist_ready(completed_short));
    }

    /// The writer keeps an append-only EVENT history, but a client must never
    /// be offered names retention already unlinked. This pure pin also covers
    /// the HLS shape: a playlist that removes old entries is a sliding media
    /// playlist, not an EVENT playlist, and its first sequence advances.
    #[test]
    fn served_live_playlist_advances_past_the_pruned_prefix() {
        let raw = "#EXTM3U\n\
                   #EXT-X-VERSION:7\n\
                   #EXT-X-TARGETDURATION:10\n\
                   #EXT-X-MEDIA-SEQUENCE:0\n\
                   #EXT-X-PLAYLIST-TYPE:EVENT\n\
                   #EXT-X-MAP:URI=\"init.mp4\"\n\
                   #EXTINF:4.000,\n\
                   seg00000.m4s\n\
                   #EXTINF:10.500,\n\
                   seg00001.m4s\n\
                   #EXT-X-DISCONTINUITY\n\
                   #EXTINF:6.000,\n\
                   seg00002.m4s\n\
                   #EXT-X-ENDLIST\n";

        assert_eq!(
            served_live_playlist(raw.as_bytes().to_vec(), Some(0), false),
            raw.as_bytes(),
            "before pruning the client sees the writer's EVENT playlist unchanged"
        );

        let served = String::from_utf8(served_live_playlist(
            raw.as_bytes().to_vec(),
            Some(2),
            false,
        ))
        .expect("playlist utf8");
        assert!(served.contains("#EXT-X-MEDIA-SEQUENCE:2"), "{served}");
        assert!(!served.contains("#EXT-X-PLAYLIST-TYPE:EVENT"), "{served}");
        assert!(!served.contains("seg00000.m4s"), "{served}");
        assert!(!served.contains("seg00001.m4s"), "{served}");
        assert!(
            served.contains("#EXT-X-DISCONTINUITY\n#EXTINF:6.000,\nseg00002.m4s"),
            "tags attached to the first retained segment survive: {served}"
        );
        assert!(served.ends_with("seg00002.m4s\n#EXT-X-ENDLIST\n"));
    }

    /// The experiment's promise is not merely that EVENT disappears after a
    /// prune; it is that one session URL presents the same typeless envelope
    /// before and after that boundary. Only MEDIA-SEQUENCE and the retained
    /// body are allowed to advance.
    #[test]
    fn typeless_sliding_playlist_keeps_one_shape_across_pruning() {
        let raw = "#EXTM3U\n\
                   #EXT-X-VERSION:7\n\
                   #EXT-X-TARGETDURATION:10\n\
                   #EXT-X-MEDIA-SEQUENCE:0\n\
                   #EXT-X-PLAYLIST-TYPE:EVENT\n\
                   #EXT-X-MAP:URI=\"init.mp4\"\n\
                   #EXTINF:4.000,\n\
                   seg00000.m4s\n\
                   #EXTINF:4.000,\n\
                   seg00001.m4s\n\
                   #EXTINF:4.000,\n\
                   seg00002.m4s\n";
        let before =
            String::from_utf8(served_live_playlist(raw.as_bytes().to_vec(), Some(0), true))
                .expect("before utf8");
        let after = String::from_utf8(served_live_playlist(raw.as_bytes().to_vec(), Some(2), true))
            .expect("after utf8");

        for playlist in [&before, &after] {
            assert!(
                !playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
                "{playlist}"
            );
            assert!(
                playlist.contains("#EXT-X-START:TIME-OFFSET=0"),
                "{playlist}"
            );
        }
        let stable_headers = |playlist: &str| {
            playlist
                .lines()
                .take_while(|line| !line.starts_with("#EXTINF:"))
                .filter(|line| !line.starts_with("#EXT-X-MEDIA-SEQUENCE:"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(stable_headers(&before), stable_headers(&after));
        assert!(before.contains("#EXT-X-MEDIA-SEQUENCE:0"), "{before}");
        assert!(after.contains("#EXT-X-MEDIA-SEQUENCE:2"), "{after}");
        assert!(before.contains("seg00000.m4s"), "{before}");
        assert!(!after.contains("seg00000.m4s"), "{after}");
        assert!(after.contains("seg00002.m4s"), "{after}");
    }

    // ---- the append-oriented index (review §2.6) ----------------------------

    /// The steady state: a grown playlist APPENDS to the index. Known
    /// entries keep their measured sizes without being re-parsed, and the new
    /// entry's timeline continues from the held cursor.
    #[test]
    fn the_index_appends_what_is_new_and_keeps_what_it_measured() {
        let two = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:3.0,\nseg00001.ts\n";
        let mut index = SegmentIndex {
            segs: parse_playlist(two),
        };
        index.segs[0].bytes = 111;
        index.segs[1].bytes = 222;

        let three = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:3.0,\nseg00001.ts\n\
                     #EXTINF:2.5,\nseg00002.ts\n";
        assert!(
            !index.extend_from_playlist(three),
            "an append, not a rebuild"
        );
        assert_eq!(index.segs.len(), 3);
        assert_eq!(
            (index.segs[0].bytes, index.segs[1].bytes),
            (111, 222),
            "measured sizes stay put"
        );
        assert_eq!(
            (index.segs[2].start_ms, index.segs[2].end_ms),
            (5_000, 7_500),
            "the new entry continues the held timeline"
        );
        assert_eq!(
            index.segs[2].bytes, 0,
            "and is not pretended to be measured"
        );

        // Nothing new: still not a rebuild, still three.
        assert!(!index.extend_from_playlist(three));
        assert_eq!(index.segs.len(), 3);
    }

    /// A playlist with fewer entries than the index is a truncation or a
    /// recovery; what is held describes files that are gone. Rebuild, and
    /// drop the carried sizes with the timeline they measured.
    #[test]
    fn a_shrunken_playlist_rebuilds_the_index() {
        let three = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:2.0,\nseg00001.ts\n\
                     #EXTINF:2.0,\nseg00002.ts\n";
        let mut index = SegmentIndex {
            segs: parse_playlist(three),
        };
        index.segs[0].bytes = 999;
        let one = "#EXTM3U\n#EXTINF:4.0,\nseg00000.ts\n";
        assert!(index.extend_from_playlist(one), "a shrink is a rebuild");
        assert_eq!(index.segs.len(), 1);
        assert_eq!(index.segs[0].end_ms, 4_000, "the new timeline, not the old");
        assert_eq!(
            index.segs[0].bytes, 0,
            "carried sizes went with the old one"
        );
    }

    /// The fallback respawn clears the directory and rewrites the timeline
    /// from the same seek point — same names, same indices, different cut
    /// points. The sentinel is the last known entry's duration: when it
    /// disagrees, everything held describes a timeline that no longer
    /// exists.
    #[test]
    fn a_replaced_timeline_is_caught_by_its_last_known_entry() {
        let two = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:2.0,\nseg00001.ts\n";
        let mut index = SegmentIndex {
            segs: parse_playlist(two),
        };
        index.segs[1].bytes = 555;
        // Same names, but the second segment now cuts at 5s, and a third
        // exists. A blind append would graft a new timeline onto a stale one.
        let replaced = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:5.0,\nseg00001.ts\n\
                        #EXTINF:2.0,\nseg00002.ts\n";
        assert!(
            index.extend_from_playlist(replaced),
            "the disagreement rebuilds"
        );
        assert_eq!(index.segs.len(), 3);
        assert_eq!(
            (index.segs[1].start_ms, index.segs[1].end_ms),
            (2_000, 7_000),
            "the rewritten cut, not the remembered one"
        );
        assert_eq!(index.segs[1].bytes, 0, "and no stale size survives it");
    }

    /// The two byte figures answer different questions (review §2.7): the
    /// AHEAD figure is the client's reserve, the TOTAL is what the disk
    /// actually holds — the retention window behind the frontier is on disk
    /// too, and summing reserves let several healthy sessions blow through
    /// the documented scratch cap by a whole window each.
    #[test]
    fn the_disk_budget_counts_retained_bytes_the_reserve_does_not() {
        let mut index = SegmentIndex {
            segs: (0..10)
                .map(|i| SegmentMeta {
                    index: i,
                    name: format!("seg{i:05}.ts"),
                    start_ms: i * 4_000,
                    end_ms: (i + 1) * 4_000,
                    bytes: 1_000_000,
                    pruned: false,
                })
                .collect(),
        };
        // Frontier at 20s: five segments ahead of it, five behind it —
        // retained for scrubbing, and every one of them still on disk.
        let ahead = ahead_of(&index, 20_000).expect("published");
        assert_eq!(ahead.bytes, 5_000_000, "the reserve is what pacing sees");
        assert_eq!(
            index.total_bytes(),
            10_000_000,
            "the disk holds the retention window too — the cap must count it"
        );
        // Retention deletes two; the budget follows the disk down.
        for seg in index.segs.iter_mut().take(2) {
            seg.bytes = 0;
            seg.pruned = true;
        }
        assert_eq!(index.total_bytes(), 8_000_000);
        assert_eq!(
            ahead_of(&index, 20_000).expect("published").bytes,
            5_000_000,
            "and the reserve never noticed — different questions"
        );
    }

    /// Flow control's limits come from the snapshot while it is fresh: an
    /// admin's change lands within the TTL, and the hot path stops paying
    /// three settings reads per segment event.
    #[tokio::test]
    async fn flow_limits_are_snapshotted_until_stale() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );

        let first = mgr.ahead_limits().await;
        assert_eq!(first.max_secs, HLS_AHEAD_MAX_SECS_DEFAULT);

        store
            .put_setting(keys::HLS_AHEAD_MAX_SECS, "999")
            .await
            .expect("setting");
        assert_eq!(
            mgr.ahead_limits().await.max_secs,
            HLS_AHEAD_MAX_SECS_DEFAULT,
            "within the TTL the snapshot answers — that is the whole point"
        );
        mgr.forget_cached_limits();
        assert_eq!(
            mgr.ahead_limits().await.max_secs,
            999,
            "a stale snapshot re-reads the settings"
        );
    }

    /// A pruned segment's file is deleted on purpose; the flag is what stops
    /// every later refresh from re-statting it forever, and an extend must
    /// not lose it.
    #[test]
    fn a_pruned_segment_stays_pruned_through_an_append() {
        let two = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:2.0,\nseg00001.ts\n";
        let mut index = SegmentIndex {
            segs: parse_playlist(two),
        };
        index.segs[0].bytes = 0;
        index.segs[0].pruned = true;
        let three = "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n#EXTINF:2.0,\nseg00001.ts\n\
                     #EXTINF:2.0,\nseg00002.ts\n";
        assert!(!index.extend_from_playlist(three));
        assert!(index.segs[0].pruned, "still pruned");
        assert!(!index.segs[2].pruned, "the new entry is not");
    }

    #[test]
    fn ahead_is_measured_against_the_fetched_frontier() {
        let index = SegmentIndex {
            segs: (0..10)
                .map(|i| SegmentMeta {
                    index: i,
                    name: format!("seg{i:05}.ts"),
                    start_ms: i * 4_000,
                    end_ms: (i + 1) * 4_000,
                    bytes: 1_000_000,
                    pruned: false,
                })
                .collect(),
        };
        // 40s published, nothing fetched → the whole 40s is reserve.
        let a = ahead_of(&index, 0).expect("published");
        assert_eq!(a.seconds, 40);
        assert_eq!(a.bytes, 10_000_000);
        // Frontier at 16s → 24s and six segments of reserve.
        let a = ahead_of(&index, 16_000).expect("published");
        assert_eq!(a.seconds, 24);
        assert_eq!(a.bytes, 6_000_000);
        // Caught up entirely.
        let a = ahead_of(&index, 40_000).expect("published");
        assert_eq!((a.seconds, a.bytes), (0, 0));
        // Nothing published is not "zero ahead" — it is "don't know".
        assert_eq!(ahead_of(&SegmentIndex::default(), 0), None);
    }

    #[test]
    fn suspend_holds_on_any_limit_and_releases_on_all() {
        let limits = AheadLimits {
            max_secs: 180,
            max_bytes: 2_000,
            global_max_bytes: 8_000,
        };
        let secs = |n| Ahead {
            seconds: n,
            bytes: 0,
        };
        let bytes = |n| Ahead {
            seconds: 0,
            bytes: n,
        };
        assert_eq!(time_release_threshold(180), Some(150));
        assert_eq!(time_release_threshold(60), Some(30));
        assert_eq!(time_release_threshold(1), Some(1));
        assert_eq!(time_release_threshold(0), None);
        // Running: hold once past any single window.
        assert!(!should_suspend(secs(179), 0, 0, limits, false));
        assert!(should_suspend(secs(181), 0, 0, limits, false));
        assert!(should_suspend(bytes(2_001), 0, 0, limits, false));
        // The global budget holds a session that is individually well behaved
        // — several healthy 4K streams fill a disk between them.
        assert!(should_suspend(secs(10), 8_001, 0, limits, false));

        // Held: time keeps its 30-second release gap so a fast encoder does
        // not toggle once per client segment near the ceiling.
        assert!(should_suspend(secs(151), 0, 0, limits, true));
        assert!(!should_suspend(secs(150), 0, 0, limits, true));
        assert!(!should_suspend(secs(138), 0, 0, limits, true));
        assert!(
            should_suspend(secs(186), 0, 0, limits, true),
            "without a client fetch, the producer can remain held"
        );
        // The global byte cap enters on total scratch but releases on the
        // drainable reserve across all sessions. Retention behind the client
        // is intentionally absent from this half-cap comparison.
        assert!(should_suspend(secs(10), 8_001, 4_001, limits, true));
        assert!(!should_suspend(secs(10), 8_001, 4_000, limits, true));
        // Time is fine but bytes are not: still held.
        assert!(should_suspend(
            Ahead {
                seconds: 10,
                bytes: 1_001
            },
            0,
            0,
            limits,
            true
        ));

        // A disabled limit never suspends, whatever its number.
        let off = AheadLimits {
            max_secs: 0,
            max_bytes: 0,
            global_max_bytes: 0,
        };
        assert!(!should_suspend(secs(10_000), 1 << 40, 1 << 40, off, false));
        assert!(!should_suspend(secs(10_000), 1 << 40, 1 << 40, off, true));

        assert_eq!(
            ahead_hold(secs(151), 0, 0, limits, true),
            Some(AheadHold {
                reason: AheadHoldReason::Time,
                release_value: 150,
            })
        );
        assert_eq!(
            ahead_hold(bytes(1_001), 0, 0, limits, true),
            Some(AheadHold {
                reason: AheadHoldReason::Bytes,
                release_value: 1_000,
            })
        );
        assert_eq!(
            ahead_hold(secs(10), 8_001, 4_001, limits, true),
            Some(AheadHold {
                reason: AheadHoldReason::Global,
                release_value: 4_000,
            })
        );
    }

    /// A producer with no request-side evaluation can run until the repair
    /// tick. Pin that gap at the shipped pace so a future cadence or pacing
    /// change cannot quietly restore the observed +120-second overshoot.
    #[test]
    fn default_repair_cadence_bounds_time_overshoot() {
        let overshoot =
            (FLOW_CONTROL_REPAIR_INTERVAL.as_secs_f64() * HLS_READRATE_DEFAULT).ceil() as i64;
        assert_eq!(FLOW_CONTROL_REPAIR_INTERVAL, Duration::from_secs(15));
        assert_eq!(HLS_READRATE_DEFAULT, 2.0);
        assert_eq!(overshoot, 30, "default worst-case overshoot in seconds");
    }

    /// Four retention floors can sit above the old half-cap release line even
    /// after every client has fetched through the published frontier. Those
    /// bytes are real disk usage, so they still trigger the cap while running;
    /// they are not drainable reserve, so they cannot keep a held producer
    /// stopped forever.
    #[test]
    fn retained_floors_cannot_make_global_release_unreachable() {
        let indexes: Vec<SegmentIndex> = (0..4)
            .map(|session| SegmentIndex {
                segs: (0..3)
                    .map(|segment| SegmentMeta {
                        index: segment,
                        name: format!("s{session}-seg{segment}.m4s"),
                        start_ms: segment * 4_000,
                        end_ms: (segment + 1) * 4_000,
                        bytes: 500,
                        pruned: false,
                    })
                    .collect(),
            })
            .collect();
        let global_live: i64 = indexes.iter().map(SegmentIndex::total_bytes).sum();
        let global_ahead: i64 = indexes
            .iter()
            .map(|index| ahead_of(index, 12_000).expect("published").bytes)
            .sum();
        assert_eq!(global_live, 6_000, "retention floors remain on disk");
        assert_eq!(global_ahead, 0, "every client drained its reserve");

        let limits = AheadLimits {
            max_secs: 180,
            max_bytes: 2_000,
            global_max_bytes: 8_000,
        };
        assert_eq!(
            ahead_hold(Ahead::default(), global_live, global_ahead, limits, true),
            None,
            "retained floors above the 4,000-byte release line are not a permanent hold"
        );
    }

    #[test]
    fn segment_index_parsing() {
        assert_eq!(segment_index("seg00000.ts"), Some(0));
        assert_eq!(segment_index("seg00042.m4s"), Some(42));
        assert_eq!(segment_index("seg12345.ts"), Some(12345));
        assert_eq!(segment_index("init.mp4"), None);
        assert_eq!(segment_index("index.m3u8"), None);
        assert_eq!(segment_index("seg.ts"), None);
    }

    #[tokio::test]
    async fn hls_codec_metadata_matches_copy_and_audio_conversion() {
        use plurx_core::domain::AudioStream;
        use plurx_core::store::SqliteStore;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let mut file = store
            .get_file(file_id)
            .await
            .expect("read file")
            .expect("seeded file");
        file.hdr = Some("dolby_vision".into());
        file.audio_streams = vec![AudioStream {
            index: 0,
            codec: "eac3".into(),
            channels: Some(6),
            language: Some("eng".into()),
            title: None,
            default: true,
        }];

        assert_eq!(
            copied_hls_codecs(
                &file,
                None,
                CopySessionOptions {
                    transcode_audio: false,
                    preserve_dolby_vision: true,
                },
                Some(
                    r#"{"streams":[{"codec_type":"video","profile":"Main 10","level":150,"side_data_list":[{"side_data_type":"DOVI configuration record","dv_profile":8,"dv_level":6,"dv_bl_signal_compatibility_id":1}]}]}"#
                ),
            ),
            (
                "hvc1.2.4.L150.B0,ec-3".to_owned(),
                Some("dvh1.08.06/db1p".to_owned())
            )
        );
        assert_eq!(
            copied_hls_codecs(
                &file,
                None,
                CopySessionOptions {
                    transcode_audio: true,
                    preserve_dolby_vision: false,
                },
                None,
            ),
            ("hvc1,mp4a.40.2".to_owned(), None)
        );

        file.hdr = None;
        file.video_codec = Some("h264".into());
        assert_eq!(
            copied_hls_codecs(
                &file,
                None,
                CopySessionOptions {
                    transcode_audio: true,
                    preserve_dolby_vision: false,
                },
                Some(r#"{"streams":[{"codec_type":"video","profile":"High","level":50}]}"#,),
            ),
            ("avc1.640032,mp4a.40.2".to_owned(), None)
        );
    }

    /// A session with no encoder behind it, for exercising the index, the
    /// retention window and the pruner without spawning ffmpeg. The child is a
    /// real (idle) process because `Session` owns one; nothing here signals it.
    fn test_session(dir: PathBuf) -> Session {
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn placeholder child");
        Session {
            dir,
            child: Mutex::new(Some(child)),
            cached: false,
            _cache_reader: None,
            last_request: Mutex::new(LastRequest::now("test-start")),
            file_id: 1,
            item_id: 1,
            item_title: "T".into(),
            user_name: "paul".into(),
            playback_id: "pb-test".into(),
            method: crate::delivery::Method::Transcode,
            start_seconds: 0.0,
            media_origin_seconds: 0.0,
            hls_codecs: "avc1.640034,mp4a.40.2".into(),
            hls_supplemental_codecs: None,
            target_height: 720,
            encoder_label: Mutex::new("test"),
            started_unix: 0,
            failed: AtomicBool::new(false),
            playlist_published: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            live_bytes: AtomicI64::new(0),
            progress: Arc::new(Progress::new()),
            class: std::sync::Mutex::new(String::new()),
            hw_slot: std::sync::Mutex::new(None),
            sw_permit: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            readrate: 0.0,
            suspended: AtomicBool::new(false),
            suspended_at: Mutex::new(None),
            suspend_count: AtomicU64::new(0),
            typeless_sliding: false,
            first_slide_logged: AtomicBool::new(false),
        }
    }

    /// Build a session directory with a real playlist and real files, so the
    /// index, the retention window and the pruner are all exercised against
    /// what ffmpeg actually writes.
    async fn seeded_session_dir(dir: &std::path::Path, count: i64, secs_each: f64) {
        let mut playlist = String::from(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-PLAYLIST-TYPE:EVENT\n",
        );
        for i in 0..count {
            playlist.push_str(&format!("#EXTINF:{secs_each:.3},\nseg{i:05}.ts\n"));
            tokio::fs::write(dir.join(format!("seg{i:05}.ts")), vec![b'x'; 1024])
                .await
                .expect("write seg");
        }
        tokio::fs::write(dir.join("index.m3u8"), playlist)
            .await
            .expect("write playlist");
    }

    /// The manager must hold the first live response while ffmpeg has only a
    /// one-segment EVENT playlist, then release it as soon as a second segment
    /// provides the startup cushion. This pins both the asynchronous polling
    /// behavior and the per-session one-way publication state.
    #[tokio::test]
    async fn first_live_transcode_playlist_waits_for_two_segments() {
        use plurx_core::store::SqliteStore;

        let dir = tempfile::tempdir().expect("tempdir");
        seeded_session_dir(dir.path(), 1, 2.0).await;
        let session = Arc::new(test_session(dir.path().to_path_buf()));
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let mgr = TranscodeManager::new(
            store,
            dir.path().join("manager-work"),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        mgr.sessions
            .lock()
            .await
            .insert("startup-gate".into(), Arc::clone(&session));

        let held =
            tokio::time::timeout(Duration::from_millis(250), mgr.playlist("startup-gate")).await;
        assert!(
            held.is_err(),
            "one segment must not escape the startup gate"
        );
        assert!(!session.playlist_published.load(Relaxed));

        seeded_session_dir(dir.path(), 2, 2.0).await;
        let playlist = tokio::time::timeout(Duration::from_secs(2), mgr.playlist("startup-gate"))
            .await
            .expect("two-segment playlist should be released")
            .expect("playlist");
        let text = String::from_utf8(playlist).expect("utf8 playlist");
        assert!(text.contains("seg00000.ts"));
        assert!(text.contains("seg00001.ts"));
        assert!(session.playlist_published.load(Relaxed));
    }

    /// Re-evaluating an unchanged running session must be a no-op. Without the
    /// state guard, a healthy playlist poll sends SIGCONT and touches the
    /// motion clock, which prevents the stall watchdog from ever firing.
    #[tokio::test]
    async fn unchanged_flow_control_state_does_not_touch_the_motion_clock() {
        use plurx_core::store::SqliteStore;
        let dir = tempfile::tempdir().expect("tempdir");
        seeded_session_dir(dir.path(), 1, 4.0).await;
        let session = test_session(dir.path().to_path_buf());
        session.refresh_segments().await;
        session.progress.moved_at_ms.store(-10_000, Relaxed);
        let before = session.progress.stalled_for();

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let mgr = TranscodeManager::new(
            store,
            dir.path().join("manager-work"),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        mgr.apply_ahead_window(
            &session,
            "unchanged-running",
            AheadLimits {
                max_secs: 180,
                max_bytes: 2_000_000_000,
                global_max_bytes: 8_000_000_000,
            },
            0,
            0,
        )
        .await;

        assert!(!session.suspended.load(Relaxed));
        assert!(
            session.progress.stalled_for() >= before,
            "an unchanged flow-control evaluation must not reset motion"
        );
    }

    /// Retention is measured back from the DOWNLOAD frontier and must leave a
    /// reload margin behind the observed playhead. This asserts the segment set
    /// that actually survives pruning, rather than only restating the constants
    /// used to calculate the window.
    #[tokio::test]
    async fn retention_keeps_a_reload_margin_above_the_observed_fetch_lead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        // 100 segments of 4s = 400s of media.
        seeded_session_dir(p, 100, 4.0).await;
        tokio::fs::write(p.join("init.mp4"), b"i")
            .await
            .expect("write init");

        let session = Arc::new(test_session(p.to_path_buf()));
        session.refresh_segments().await;
        assert_eq!(
            session.segments.lock().await.produced_playable_end_ms(),
            Some(400_000)
        );

        // Physical-iPad regression: AVPlayer fetched through 300s while its
        // playhead was around 180s, despite a 60s preferred forward buffer.
        // The retained window must still leave 60s behind that playhead for
        // back-buffering and a retry.
        let observed_playhead_ms = 180_000;
        let observed_fetched_end_ms = 300_000;
        let required_reload_margin_ms = 60_000;
        session
            .fetched_end_ms
            .store(observed_fetched_end_ms, Relaxed);
        gc_expired_segments(&session).await;

        // The retained/pruned segment boundary is the behavior under test.
        // Segment 30 begins at 120s, leaving 60s behind the observed 180s
        // playhead even though the download frontier had reached 300s.
        let (first_retained, retained_start_ms) = {
            let index = session.segments.lock().await;
            let first = index.first_retained_index().expect("retained segment");
            let (start, _) = index.window_ms_of(first).expect("retained window");
            (first, start)
        };
        assert_eq!(first_retained, 30);
        assert_eq!(
            observed_playhead_ms - retained_start_ms,
            required_reload_margin_ms,
            "the actual retained set leaves the required reload margin"
        );
        assert!(!p.join("seg00029.ts").exists(), "just before the boundary");
        assert!(p.join("seg00030.ts").exists(), "retention boundary");
        assert!(p.join("seg00074.ts").exists(), "just inside the window");
        // …and only what is older goes.
        assert!(!p.join("seg00000.ts").exists());
        assert!(!p.join("seg00010.ts").exists());
        assert!(p.join("init.mp4").exists(), "init is never a segment");

        // The writer's history remains complete for duration accounting, but
        // the manager's HTTP-facing view advances past every deleted URI.
        let raw = tokio::fs::read_to_string(p.join("index.m3u8"))
            .await
            .expect("raw playlist");
        assert!(raw.contains("#EXT-X-PLAYLIST-TYPE:EVENT"), "{raw}");
        assert!(raw.contains("seg00000.ts"), "{raw}");

        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let mgr = TranscodeManager::new(
            store,
            p.join("manager-work"),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        mgr.sessions
            .lock()
            .await
            .insert("retained-window".into(), Arc::clone(&session));
        let served = String::from_utf8(
            mgr.playlist("retained-window")
                .await
                .expect("served playlist"),
        )
        .expect("playlist utf8");
        assert!(served.contains("#EXT-X-MEDIA-SEQUENCE:30"), "{served}");
        assert!(!served.contains("#EXT-X-PLAYLIST-TYPE:EVENT"), "{served}");
        assert!(!served.contains("seg00029.ts"), "{served}");
        assert!(served.contains("seg00030.ts"), "{served}");
        for name in served.lines().filter(|line| line.ends_with(".ts")) {
            assert!(p.join(name).exists(), "served segment must exist: {name}");
        }

        // Subtitle timing still sees the discarded prefix through the full
        // internal index: segment 30 begins at 120 seconds, not at zero.
        assert_eq!(
            mgr.segment_window("retained-window", 30).await,
            Some((120.0, 124.0))
        );

        // A frontier that has not yet passed the window prunes nothing.
        let fresh = tempfile::tempdir().expect("tempdir");
        seeded_session_dir(fresh.path(), 10, 4.0).await;
        let young = test_session(fresh.path().to_path_buf());
        young.refresh_segments().await;
        young.fetched_end_ms.store(40_000, Relaxed);
        gc_expired_segments(&young).await;
        assert!(fresh.path().join("seg00000.ts").exists());
    }

    /// The reserve in bytes has to shrink when the bytes stop existing, or the
    /// budget would hold a session for scratch it already reclaimed.
    #[tokio::test]
    async fn pruned_bytes_stop_counting_toward_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        seeded_session_dir(dir.path(), 100, 4.0).await;
        let session = test_session(dir.path().to_path_buf());
        session.refresh_segments().await;
        session.fetched_end_ms.store(300_000, Relaxed);

        let before = session.ahead().await.expect("published").bytes;
        gc_expired_segments(&session).await;
        let after = session.ahead().await.expect("published").bytes;
        assert_eq!(
            before, after,
            "pruning happens BEHIND the frontier, so the ahead figure is untouched"
        );
        // What did change is total scratch, which a refresh re-measures.
        session.refresh_segments().await;
        let total: i64 = session
            .segments
            .lock()
            .await
            .segs
            .iter()
            .map(|s| s.bytes)
            .sum();
        assert!(total < 100 * 1024, "pruned files no longer count: {total}");
    }

    /// Write a real (tiny) H.264 file, because the fixtures elsewhere are
    /// deliberately garbage bytes and ffmpeg refuses them — which is fine when
    /// the assertion is about bookkeeping, and useless when it is about
    /// telemetry that only exists while ffmpeg is genuinely working.
    fn write_real_video(path: &std::path::Path, seconds: u32) {
        let status = std::process::Command::new(
            std::env::var("PLURX_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
        )
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=160x120:rate=15:duration={seconds}"),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "15",
            "-y",
        ])
        .arg(path)
        .status();
        assert!(
            status.map(|s| s.success()).unwrap_or(false),
            "fixture encode failed — this test needs a working ffmpeg"
        );
    }

    /// The whole point of the telemetry is that it comes from a running
    /// encoder, so this drives one: real input, real `-progress` stream, real
    /// signals. It is the only test that can catch the plumbing being wrong —
    /// a stdout that was never piped, a progress flag ffmpeg ignored, a
    /// SIGSTOP sent to the wrong pid — because every one of those looks
    /// perfectly healthy from the outside.
    #[tokio::test]
    async fn a_client_fetch_releases_a_held_session_and_restarts_progress() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let media = tempfile::tempdir().expect("media dir");
        let src = media.path().join("clip.mp4");
        write_real_video(&src, 60);

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file_at(&store, &src.to_string_lossy()).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        // Burst then crawl. The burst gives the session something to be ahead
        // *with* straight away; the realtime pace afterwards keeps it alive for
        // the rest of the test, because an unpaced copy of a short clip
        // finishes before there is anything left to suspend.
        store.put_setting(keys::HLS_READRATE, "1").await.expect("s");
        store
            .put_setting(keys::HLS_BURST_SECS, "20")
            .await
            .expect("s");
        store
            .put_setting(keys::HLS_AHEAD_MAX_SECS, "1")
            .await
            .expect("s");
        store
            .put_setting(keys::HLS_AHEAD_MAX_BYTES, "0")
            .await
            .expect("s");
        store
            .put_setting(keys::HLS_SCRATCH_MAX_BYTES, "0")
            .await
            .expect("s");

        let info = mgr
            .start_copy(
                file_id,
                0.0,
                None,
                CopySessionOptions {
                    transcode_audio: false,
                    preserve_dolby_vision: false,
                },
                "paul",
                "pb-paul",
            )
            .await
            .expect("copy session");
        let session = mgr
            .sessions
            .lock()
            .await
            .get(&info.session_id)
            .cloned()
            .expect("session is tracked");

        // Telemetry arrives from the real progress stream, and advances. (The
        // first block can legitimately read zero — a frame at PTS 0 — so this
        // waits for movement, not merely for a number.)
        let produced = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match session.progress.out_time_ms() {
                    Some(ms) if ms >= 3_000 => return ms,
                    _ => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        })
        .await
        .expect("ffmpeg reported an advancing output timeline");
        assert!(produced >= 3_000);
        assert!(
            session.progress.speed().is_some(),
            "the speed field parses too — it is what tells slow from stuck"
        );

        // Nothing has been fetched, so everything published is reserve. This
        // reads the index (what the playlist says exists) rather than the
        // encoder clock, so it also proves the parser sees real ffmpeg output.
        session.refresh_segments().await;
        assert!(
            session
                .ahead()
                .await
                .is_some_and(|a| a.seconds > 0 && a.bytes > 0),
            "published media with an unmoved frontier is reserve"
        );

        // A window it has already exceeded suspends it, and a suspended
        // encoder stops advancing — which is the property the watchdog relies
        // on being able to tell apart from a wedge.
        mgr.flow_control(&session, &info.session_id).await;
        assert!(session.suspended.load(Relaxed), "session was held");
        let status = mgr.session_status(&info.session_id).await.expect("status");
        assert_eq!(status.readrate, 1.0, "HLS exposes its effective input pace");
        assert_eq!(status.hold_reason, Some(AheadHoldReason::Time));
        assert_eq!(status.resume_below_seconds, Some(1));
        assert_eq!(status.resume_below_bytes, None);
        assert_eq!(status.suspend_count, 1, "the first transition is visible");
        let frozen = session.progress.out_time_ms();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            session.progress.out_time_ms(),
            frozen,
            "a suspended encoder produces nothing"
        );

        // Fetch the newest published segment through the real request path.
        // That advances the download frontier, re-evaluates the same configured
        // limit, sends SIGCONT, and restarts actual encoder progress.
        let newest = session
            .segments
            .lock()
            .await
            .segs
            .last()
            .expect("published segment")
            .name
            .clone();
        assert!(mgr.segment(&info.session_id, &newest).await.is_some());
        assert!(!session.suspended.load(Relaxed), "session was released");
        assert_eq!(
            mgr.session_status(&info.session_id)
                .await
                .expect("status after release")
                .suspend_count,
            1,
            "resume preserves the transition history"
        );
        let flow_events = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let events = store
                    .playback_events(&plurx_core::domain::PlaybackEventQuery {
                        since_ms: None,
                        event: None,
                        limit: 20,
                    })
                    .await
                    .expect("flow-control telemetry query");
                if events.iter().any(|event| event.event == "suspend")
                    && events.iter().any(|event| event.event == "resume")
                {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("suspend/resume telemetry persisted");
        let suspend = flow_events
            .iter()
            .find(|event| event.event == "suspend")
            .expect("suspend row");
        assert_eq!(
            suspend.session_id.as_deref(),
            Some(info.session_id.as_str())
        );
        assert_eq!(suspend.hold_reason.as_deref(), Some("time"));
        assert_eq!(suspend.readrate, Some(1.0));
        let resume = flow_events
            .iter()
            .find(|event| event.event == "resume")
            .expect("resume row");
        assert_eq!(resume.hold_reason.as_deref(), Some("time"));
        assert!(
            resume.ms.is_some_and(|held_ms| held_ms >= 1_000),
            "resume row carries the measured hold duration: {:?}",
            resume.ms
        );
        let moved = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if session.progress.out_time_ms() > frozen {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(moved, "SIGCONT actually restarted the encoder");

        // A suspended child still dies on request — SIGKILL does not need the
        // process to be scheduled, which is what makes the reaper safe.
        let republished = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                session.refresh_segments().await;
                if session.ahead().await.is_some_and(|ahead| ahead.bytes > 1) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(republished, "the resumed encoder published another segment");
        mgr.apply_ahead_window(
            &session,
            &info.session_id,
            AheadLimits {
                max_secs: 0,
                max_bytes: 1,
                global_max_bytes: 0,
            },
            0,
            0,
        )
        .await;
        assert!(session.suspended.load(Relaxed), "held again");
        assert_eq!(
            mgr.session_status(&info.session_id)
                .await
                .expect("status after second hold")
                .suspend_count,
            2,
            "a second transition cannot hide between activity polls"
        );
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert_eq!(mgr.active_sessions().await, 0);
    }

    async fn seed_file(store: &Arc<dyn Store>) -> i64 {
        seed_file_at(store, "/media/Heat.mkv").await
    }

    async fn seed_file_at(store: &Arc<dyn Store>, path: &str) -> i64 {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = store
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
            .expect("movie");
        store
            .upsert_file(
                movie,
                path,
                1,
                1,
                &ProbeResult {
                    duration_ms: Some(6_000_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(3840),
                    height: Some(2160),
                    ..Default::default()
                },
            )
            .await
            .expect("file")
    }

    /// The cap has to hold against *concurrent* starts, which is the only case
    /// that matters: a third 4K session admitted alongside two others does not
    /// run a third as fast, it drags all three under realtime. A count read and
    /// then written would let every racer through.
    #[tokio::test]
    async fn concurrent_starts_cannot_exceed_the_hardware_slot_cap() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        // A box that believes it has NVENC. The encode will fail (there is no
        // GPU here) but admission happens before the spawn, which is the point.
        let mgr = Arc::new(TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps {
                nvenc: true,
                ..Default::default()
            },
            Pipeline::Cpu,
        ));
        store
            .put_setting(keys::MAX_HW_SESSIONS, "2")
            .await
            .expect("cap");

        // Five players, all at once, each with its own playback id so none
        // supersedes another.
        let mut starts = Vec::new();
        for n in 0..5 {
            let mgr = Arc::clone(&mgr);
            starts.push(tokio::spawn(async move {
                mgr.start(file_id, 1080, 0.0, None, None, "paul", &format!("pb-{n}"))
                    .await
            }));
        }
        let mut admitted = 0;
        let mut refused = 0;
        for s in starts {
            match s.await.expect("join") {
                Ok(_) => admitted += 1,
                Err(why) => {
                    // The 4K HEVC fixture is exactly the shape software cannot
                    // carry, so the overflow is refused rather than downgraded
                    // — and it says why rather than failing anonymously.
                    assert!(why.contains("hardware transcode slots"), "{why}");
                    refused += 1;
                }
            }
        }
        assert_eq!(admitted, 2, "the cap is the cap");
        assert_eq!(refused, 3);
        assert_eq!(mgr.admissions.in_use(), 2, "and it is accounted for");

        // Ending a session gives its slot back — the guard rides on the
        // session, so every way one can end returns the slot.
        let live: Vec<String> = mgr.sessions.lock().await.keys().cloned().collect();
        for id in live {
            assert!(mgr.stop_session(&id, "test").await);
        }
        assert_eq!(mgr.admissions.in_use(), 0, "slots come back");
    }

    /// The hardware→software fallback must return its slot at the transition,
    /// not at teardown: with a cap of one, the next hardware start would
    /// otherwise queue behind a software session for as long as it lives —
    /// potentially a whole film. And the admission record must flip to the
    /// software class, or every speed measured from the replacement encoder
    /// is filed as evidence about hardware.
    #[tokio::test]
    async fn a_fallback_to_software_frees_the_hardware_slot_at_once() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps {
                nvenc: true,
                ..Default::default()
            },
            Pipeline::Cpu,
        );
        store
            .put_setting(keys::MAX_HW_SESSIONS, "1")
            .await
            .expect("cap");

        let info = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-fallback")
            .await
            .expect("hardware start");
        assert_eq!(mgr.admissions.in_use(), 1, "the start holds the only slot");

        let session = mgr
            .sessions
            .lock()
            .await
            .get(&info.session_id)
            .cloned()
            .expect("session");
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let work_class = Workload::of(&file, session.target_height);
        let permit = mgr
            .admissions
            .software_pool()
            .take_forced(work_class.software_threads());
        session.demote_to_software(work_class, permit);

        assert_eq!(
            mgr.admissions.in_use(),
            0,
            "the slot came back at the transition"
        );
        assert_eq!(
            *session.class.lock().expect("class"),
            Workload::of(&file, session.target_height).software_class(),
            "speeds measured from here on are software evidence"
        );
        // The whole point, stated as the viewer experiences it: with the cap
        // at one and the demoted session still alive, the next hardware start
        // is admitted immediately instead of queuing for this session's life.
        match mgr
            .admissions
            .admit(1, Workload::of(&file, session.target_height))
        {
            Admission::Hardware(_slot) => {}
            other => panic!("the freed slot must be grantable now, got {other:?}"),
        }
        assert!(mgr.stop_session(&info.session_id, "test").await);
    }

    // ---- the software CPU pool, wired through start() (review §2.4) ---------

    /// A software-only box. Sessions used to bypass admission entirely here;
    /// now a start reserves its thread weight and every ending returns it.
    #[tokio::test]
    async fn software_starts_reserve_the_cpu_pool_and_stops_return_it() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(), // no hardware: software from the start
            Pipeline::Cpu,
        );

        let info = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-sw")
            .await
            .expect("software start");
        // The 4K fixture at a 1080 rung: base 4 threads + the 4K decode
        // surcharge — the weight is the workload's, not the machine's.
        assert_eq!(
            mgr.admissions.software_in_use(),
            6,
            "the start reserved its weight"
        );
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert_eq!(
            mgr.admissions.software_in_use(),
            0,
            "and the stop returned it"
        );
    }

    /// The budget is a bound on *joining*, not on existing: a second session
    /// that does not fit is refused with the reason, and space freed by a
    /// stop is grantable again.
    #[tokio::test]
    async fn the_software_pool_refuses_what_it_cannot_fit() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        // Policy under test, not the build machine's core count.
        store
            .put_setting(keys::SW_POOL_THREADS, "8")
            .await
            .expect("budget");

        let first = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-a")
            .await
            .expect("first fits an empty pool");
        let refused = match mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-b")
            .await
        {
            Err(why) => why,
            Ok(_) => panic!("6 + 6 exceeds a budget of 8 and must be refused"),
        };
        assert!(refused.contains("software CPU pool"), "{refused}");

        assert!(mgr.stop_session(&first.session_id, "test").await);
        let second = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-c")
            .await
            .expect("freed weight is grantable again");
        assert!(mgr.stop_session(&second.session_id, "test").await);
    }

    /// On a tiny box every session is over budget; the empty-pool exception
    /// is what keeps the budget from being a ban. One saturating session is
    /// the best that box can do.
    #[tokio::test]
    async fn a_lone_software_session_may_exceed_a_tiny_budget() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        store
            .put_setting(keys::SW_POOL_THREADS, "2")
            .await
            .expect("budget");

        let info = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-lone")
            .await
            .expect("the only session may saturate the box");
        assert_eq!(
            mgr.admissions.software_in_use(),
            6,
            "overcommit on the books"
        );
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert_eq!(mgr.admissions.software_in_use(), 0);
    }

    // ---- the lifetime watchdog (review §2.3) --------------------------------

    /// Every state that is not a stall, and the one that is. Each `Wait` here
    /// is a healthy session the wrong verdict would kill mid-film.
    #[test]
    fn a_stall_is_the_only_thing_the_watchdog_calls_a_stall() {
        let long = PROGRESS_STALL + Duration::from_secs(1);
        let short = PROGRESS_STALL - Duration::from_secs(1);
        // Failed elsewhere, or exited (EOF and kills both have their own
        // reporters): the watch simply ends, whatever the clock says.
        assert_eq!(watch_next(true, false, false, long), WatchNext::Done);
        assert_eq!(watch_next(false, true, false, long), WatchNext::Done);
        // Suspended: motionless on purpose, however long it lasts.
        assert_eq!(watch_next(false, false, true, long), WatchNext::Wait);
        // Moving recently enough.
        assert_eq!(watch_next(false, false, false, short), WatchNext::Wait);
        // Running, unsuspended, and the output has sat still too long.
        assert_eq!(watch_next(false, false, false, long), WatchNext::Stall);
    }

    /// `moved_at_ms` freezes with the SIGSTOP — correctly — so without a
    /// resume re-baseline, the first check after SIGCONT reads the whole
    /// suspension as a stall and kills a session that was healthy on both
    /// sides of it.
    #[test]
    fn resume_restarts_the_motion_clock() {
        let p = Progress::new();
        p.begin_attempt();
        // A long-suspended session: last movement far in the past.
        p.moved_at_ms.store(
            p.started.elapsed().as_millis() as i64 - 10 * PROGRESS_STALL.as_millis() as i64,
            Relaxed,
        );
        assert!(
            p.stalled_for() >= PROGRESS_STALL,
            "the setup must look stalled"
        );
        p.touch();
        assert!(
            p.stalled_for() < PROGRESS_STALL,
            "after resume the clock counts from the resume, not the suspension"
        );
    }

    /// A session for driving `watch_for_stall` directly: a real (harmless)
    /// child process, a directory the test controls, and telemetry the test
    /// can backdate.
    fn watchdog_session(dir: &std::path::Path, child: Option<Child>, cached: bool) -> Arc<Session> {
        Arc::new(Session {
            dir: dir.to_path_buf(),
            child: Mutex::new(child),
            cached,
            _cache_reader: None,
            last_request: Mutex::new(LastRequest::now("test-start")),
            file_id: 1,
            item_id: 1,
            item_title: "Watchdog Fixture".into(),
            user_name: "paul".into(),
            playback_id: "pb-watchdog".into(),
            method: crate::delivery::Method::Transcode,
            start_seconds: 0.0,
            media_origin_seconds: 0.0,
            hls_codecs: "avc1.640034,mp4a.40.2".into(),
            hls_supplemental_codecs: None,
            target_height: 1080,
            encoder_label: Mutex::new("test"),
            started_unix: 0,
            failed: AtomicBool::new(false),
            playlist_published: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            live_bytes: AtomicI64::new(0),
            progress: Arc::new(Progress::new()),
            class: std::sync::Mutex::new(String::new()),
            hw_slot: std::sync::Mutex::new(None),
            sw_permit: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            readrate: 0.0,
            suspended: AtomicBool::new(false),
            suspended_at: Mutex::new(None),
            suspend_count: AtomicU64::new(0),
            typeless_sliding: false,
            first_slide_logged: AtomicBool::new(false),
        })
    }

    /// A process that outlives the test unless the watchdog kills it.
    fn long_running_child() -> Child {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60").kill_on_drop(true);
        cmd.spawn().expect("spawn sleep")
    }

    /// Backdate the motion clock so `stalled_for` reads past the threshold
    /// without the test waiting it out.
    fn force_stalled(p: &Progress) {
        p.moved_at_ms.store(
            p.started.elapsed().as_millis() as i64 - 2 * PROGRESS_STALL.as_millis() as i64,
            Relaxed,
        );
    }

    /// Drive virtual time in poll-sized hops until the watchdog settles.
    ///
    /// Hop-sized on purpose: under a paused clock, auto-advance jumps to the
    /// *nearest* pending timer whenever every task is off doing file I/O — so
    /// a single distant `timeout` wrapped around the watchdog is a deadline
    /// the clock can leap straight to while the watchdog sits in a playlist
    /// read. Small hops leave it nothing to leap past. Bounded, so a watchdog
    /// that never reaches a verdict fails the test instead of hanging it.
    async fn settle(handle: tokio::task::JoinHandle<()>) {
        for _ in 0..1_000 {
            if handle.is_finished() {
                handle.await.expect("watchdog task");
                return;
            }
            tokio::time::sleep(WATCHDOG_POLL).await;
        }
        panic!("the watchdog did not settle inside the bounded window");
    }

    /// THE §2.3 regression: the watchdog used to return at the first playable
    /// segment, so a pipeline that wedged after producing was nobody's
    /// problem — the client drained its buffer into a hang. Virtual time; the
    /// only real thing here is the child process the verdict must kill.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_outlives_the_first_segment() {
        let dir = tempfile::tempdir().expect("dir");
        // The playlist lists a finished segment: the session HAS produced.
        tokio::fs::write(
            dir.path().join("index.m3u8"),
            "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n",
        )
        .await
        .expect("playlist");
        let session = watchdog_session(dir.path(), Some(long_running_child()), false);
        force_stalled(&session.progress);

        settle(tokio::spawn(watch_for_stall(
            Arc::clone(&session),
            dir.path().to_path_buf(),
            "wd".into(),
        )))
        .await;

        assert!(
            session.failed.load(Relaxed),
            "a mid-stream wedge must fail the session even though a segment was published"
        );
        let killed = session
            .child
            .lock()
            .await
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
        assert!(killed, "and the wedged process must be gone");
    }

    /// Flow control SIGSTOPs a session that has run far enough ahead; that is
    /// health, not a stall — for as long as it lasts. And the resume path
    /// re-baselines the clock, so the suspension itself can never be read
    /// back as one.
    #[tokio::test(start_paused = true)]
    async fn a_suspended_session_is_never_judged() {
        let dir = tempfile::tempdir().expect("dir");
        tokio::fs::write(
            dir.path().join("index.m3u8"),
            "#EXTM3U\n#EXTINF:2.0,\nseg00000.ts\n",
        )
        .await
        .expect("playlist");
        let session = watchdog_session(dir.path(), Some(long_running_child()), false);
        session.suspended.store(true, Relaxed);
        force_stalled(&session.progress);

        let watchdog = tokio::spawn(watch_for_stall(
            Arc::clone(&session),
            dir.path().to_path_buf(),
            "wd".into(),
        ));
        // Long enough (in virtual time) for many verdicts, were one coming.
        tokio::time::sleep(SOFTWARE_GRACE + 20 * WATCHDOG_POLL).await;
        assert!(
            !session.failed.load(Relaxed),
            "a suspended session is motionless on purpose"
        );

        // Resume, the way `apply_ahead_window` does: clock first, flag second.
        session.progress.touch();
        session.suspended.store(false, Relaxed);
        tokio::time::sleep(WATCHDOG_POLL).await;
        assert!(
            !session.failed.load(Relaxed),
            "the suspension must not be read back as a stall at resume"
        );

        // Now a real wedge: running, unsuspended, output sits still.
        force_stalled(&session.progress);
        settle(watchdog).await;
        assert!(
            session.failed.load(Relaxed),
            "a wedge after the resume is still caught"
        );
    }

    /// EOF is success: the encoder finishing the film exits, and the watch
    /// ends without a verdict — nothing here may mark that session failed.
    #[tokio::test(start_paused = true)]
    async fn an_exited_encoder_ends_the_watch_without_a_verdict() {
        let dir = tempfile::tempdir().expect("dir");
        let mut child = tokio::process::Command::new("true")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn true");
        // Already exited before the watch begins — the deterministic version
        // of "the encoder reached EOF". (tokio caches the status, so the
        // watchdog's `try_wait` sees it.)
        child.wait().await.expect("exit");
        let session = watchdog_session(dir.path(), Some(child), false);
        // Backdated clock: if the exit check did not come first, this would
        // read as a stall — the assertion below is what makes EOF win.
        force_stalled(&session.progress);

        settle(tokio::spawn(watch_for_stall(
            Arc::clone(&session),
            dir.path().to_path_buf(),
            "wd".into(),
        )))
        .await;
        assert!(
            !session.failed.load(Relaxed),
            "an exit is EOF or a kill; both already have owners reporting them"
        );
    }

    /// A cached session has no process; every stall question about it is
    /// about nothing, and `failed` on it would poison a finished asset other
    /// viewers want.
    #[tokio::test(start_paused = true)]
    async fn a_cached_session_is_not_watched() {
        let dir = tempfile::tempdir().expect("dir");
        let session = watchdog_session(dir.path(), None, true);
        force_stalled(&session.progress);
        settle(tokio::spawn(watch_for_stall(
            Arc::clone(&session),
            dir.path().to_path_buf(),
            "wd".into(),
        )))
        .await;
        assert!(
            !session.failed.load(Relaxed),
            "nothing to watch, no verdict"
        );
    }

    // ---- the pre-transcode cache, serving side (PERF-PLAN §6.3) -------------

    const NODE: &str = "node-under-test";

    /// A manager with a cache root, and the cache root's temp dir (which has to
    /// outlive the manager or the directory goes out from under it).
    fn cached_manager(
        store: &Arc<dyn Store>,
    ) -> (TranscodeManager, tempfile::TempDir, tempfile::TempDir) {
        let work = tempfile::tempdir().expect("work");
        let cache = tempfile::tempdir().expect("cache");
        let mgr = TranscodeManager::new(
            Arc::clone(store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        )
        .with_cache(
            cache.path().to_path_buf(),
            "ffmpeg version 6.1.1-test".into(),
            NODE.into(),
        );
        (mgr, work, cache)
    }

    /// The name `start()` would look this session up under. Computed through
    /// the manager's own builders, because a test that spelled the recipe out
    /// by hand would keep passing after the two spellings diverged — which is
    /// the failure the single builder exists to prevent.
    async fn recipe_hash_for(
        mgr: &TranscodeManager,
        file: &plurx_core::domain::MediaFile,
        height: i64,
    ) -> String {
        let encoder = mgr.encoder().await;
        let opts = mgr.options_for(encoder, file, height, 0.0, None, None, None);
        let mut digest = mgr.digest().expect("cache configured");
        mgr.effective_recipe(&mut digest, file, &opts, encoder, false)
            .hash()
    }

    /// Write what a finished transcode looks like on disk.
    async fn seed_cache_dir(root: &std::path::Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        seeded_session_dir(&dir, 3, 2.0).await;
        dir
    }

    /// The three ways a lookup can go, and only one of them is a hit.
    ///
    /// The middle case is the one worth writing down: a row that says the bytes
    /// are there is not the bytes being there. A cache root on a mount that did
    /// not come back after a reboot leaves every row intact and every directory
    /// gone, and a lookup that trusted the row would hand out a playlist for an
    /// empty directory — an error the viewer sees as a film that will not play,
    /// with nothing in the log to say why.
    #[tokio::test]
    async fn a_lookup_hits_only_on_a_finished_entry_whose_bytes_are_really_there() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 1080).await;
        let encoder = mgr.encoder().await;
        let opts = mgr.options_for(encoder, &file, 1080, 0.0, None, None, None);
        let look = || mgr.serve_cached(&file, &opts, encoder, "Heat", "paul", "pb-1");

        // Nothing claimed yet.
        assert!(look().await.is_none(), "an unknown recipe is a miss");

        // Claimed but unfinished: a directory a producer is still writing into.
        let dir = seed_cache_dir(cache.path(), "ab/entry").await;
        store
            .claim_cache_entry(&hash, file_id, 1, NODE, "ab/entry")
            .await
            .expect("claim");
        assert!(
            look().await.is_none(),
            "a claim is not a hit — that playlist stops in the middle of the film"
        );

        store
            .complete_cache_entry(&hash, NODE, 1_234)
            .await
            .expect("complete");
        let hit = look().await.expect("a finished entry serves");
        assert!(
            hit.vod,
            "the whole stream exists; the player may seek freely"
        );
        assert_eq!(hit.encoder, "cached");
        assert_eq!(
            hit.start_seconds, 0.0,
            "a cached asset is the whole title — where a viewer joins is a seek"
        );
        assert_eq!(mgr.active_sessions().await, 1);
        assert!(mgr.stop_session(&hit.session_id, "test").await);

        // The row survives what the filesystem does not.
        tokio::fs::remove_file(dir.join("index.m3u8"))
            .await
            .expect("rm playlist");
        assert!(
            look().await.is_none(),
            "a row pointing at a directory with no playlist is a miss, not a 404 for the viewer"
        );
    }

    /// The whole point, end to end: a hit plays with no encoder, no hardware
    /// slot and no queue — and, the part that would destroy the cache, the
    /// bytes are still there afterwards.
    ///
    /// Every other way a session ends removes its directory, correctly. A
    /// cached session reaching one of those paths unguarded would delete the
    /// entry that served it, so each hit would cost the next viewer a full
    /// re-encode and the cache would sit permanently empty while looking like
    /// it was working.
    #[tokio::test]
    async fn a_hit_starts_no_encoder_and_outlives_the_session_that_played_it() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 1080).await;
        let dir = seed_cache_dir(cache.path(), "cd/entry").await;
        store
            .claim_cache_entry(&hash, file_id, 1, NODE, "cd/entry")
            .await
            .expect("claim");
        store
            .complete_cache_entry(&hash, NODE, 1_234)
            .await
            .expect("complete");

        // No `require_ffmpeg` here on purpose: if this ever spawns one, the
        // test should fail rather than quietly start passing on a box that has
        // ffmpeg installed.
        let info = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-1")
            .await
            .expect("start");
        assert!(info.vod);
        assert_eq!(info.encoder, "cached");
        let session = mgr
            .sessions
            .lock()
            .await
            .get(&info.session_id)
            .cloned()
            .expect("registered");
        assert!(
            session.child.lock().await.is_none(),
            "a hit has nothing to run"
        );
        assert_eq!(
            mgr.admissions.in_use(),
            0,
            "and nothing to wait behind — the work is already done"
        );
        assert!(mgr.playlist(&info.session_id).await.is_some());

        // Retention must not treat a finished asset as this session's scratch.
        // Pretend the viewer has watched well past the window and let the
        // pruner have its pass.
        session.refresh_segments().await;
        session
            .fetched_end_ms
            .store((RETENTION_SECS + 600) * 1000, Relaxed);
        gc_expired_segments(&session).await;
        assert_eq!(
            session.ahead_bytes.load(Relaxed),
            0,
            "a cache entry is not scratch, and must not push live encoders over the global budget"
        );
        assert_eq!(
            session.live_bytes.load(Relaxed),
            0,
            "on the disk-budget figure exactly as on the pacing one"
        );

        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert!(
            tokio::fs::metadata(dir.join("index.m3u8")).await.is_ok(),
            "the cache entry outlives the session that played it"
        );
        assert!(
            tokio::fs::metadata(dir.join("seg00000.ts")).await.is_ok(),
            "and so do its segments — the pruner does not eat a cached asset"
        );
        assert!(
            store.cache_hit(&hash, NODE).await.expect("hit").is_some(),
            "so the next viewer hits it too"
        );
    }

    /// Cache housekeeping and serving share one ownership registry. A budget
    /// change may leave the cache temporarily over its ceiling, but it may
    /// never remove a VOD playlist or segment while a viewer can still ask for
    /// it. The next sweep reclaims the entry after that session ends.
    #[tokio::test]
    async fn active_cache_playback_is_not_evicted_under_its_reader() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 1080).await;
        let dir = seed_cache_dir(cache.path(), "ef/entry").await;
        store
            .claim_cache_entry(&hash, file_id, 1, NODE, "ef/entry")
            .await
            .expect("claim");
        store
            .complete_cache_entry(&hash, NODE, 1_234)
            .await
            .expect("complete");
        store
            .put_setting(keys::CACHE_MAX_GB, "0")
            .await
            .expect("disable cache");

        let info = mgr
            .start(file_id, 1080, 0.0, None, None, "paul", "pb-reader")
            .await
            .expect("cached start");
        assert_eq!(info.encoder, "cached");

        let active = crate::cachekeep::sweep_with_readers(
            &store,
            cache.path(),
            NODE,
            mgr.cache_readers(),
            0,
        )
        .await;
        assert_eq!((active.evicted, active.protected), (0, 1));
        assert!(
            dir.join("index.m3u8").exists(),
            "the budget sweep removed an active viewer's playlist"
        );
        assert!(
            store.cache_hit(&hash, NODE).await.expect("hit").is_some(),
            "the active entry's row must stay with its bytes"
        );

        assert!(mgr.stop_session(&info.session_id, "test").await);
        let idle = crate::cachekeep::sweep_with_readers(
            &store,
            cache.path(),
            NODE,
            mgr.cache_readers(),
            1,
        )
        .await;
        assert_eq!((idle.evicted, idle.protected), (1, 0));
        assert!(!dir.exists(), "an idle over-budget entry was not reclaimed");
        assert!(
            store.cache_hit(&hash, NODE).await.expect("miss").is_none(),
            "eviction removed the bytes but left a serveable row"
        );
    }

    /// The producer and the serving path, against each other, with a real
    /// ffmpeg.
    ///
    /// This is the only test that can catch the two disagreeing, and the
    /// disagreement is silent: a producer that hashes its output one way and a
    /// playback that looks it up another produces a cache that fills forever
    /// and never hits, which from outside is indistinguishable from a cache
    /// that is simply cold. Every unit test above passes in that world.
    #[tokio::test]
    async fn what_the_producer_makes_is_what_a_playback_finds() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, 6);
        let file_id = seed_real_file(&store, &source).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");

        let hash = mgr
            .produce(&file, 240, Instant::now() + Duration::from_secs(120))
            .await
            .expect("produce")
            .expect("something was produced")
            .recipe;

        // On disk as one continuous, finished asset — no part directories, no
        // gaps in the numbering, and an ENDLIST so a player treats it as VOD.
        let dir = cache.path().join(&hash[..2]).join(&hash);
        let playlist = tokio::fs::read_to_string(dir.join("index.m3u8"))
            .await
            .expect("playlist");
        assert!(playlist.contains("#EXT-X-ENDLIST"), "{playlist}");
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{playlist}");
        let names: Vec<&str> = playlist.lines().filter(|l| l.ends_with(".ts")).collect();
        assert!(names.len() >= 2, "expected several segments: {playlist}");
        for (i, name) in names.iter().enumerate() {
            assert_eq!(*name, format!("seg{i:05}.ts"), "numbering has a gap");
            assert!(
                tokio::fs::metadata(dir.join(name)).await.is_ok(),
                "{name} is in the playlist but not on disk"
            );
        }
        let mut entries = tokio::fs::read_dir(&dir).await.expect("read dir");
        while let Ok(Some(e)) = entries.next_entry().await {
            assert!(
                !e.file_name().to_string_lossy().starts_with("part-"),
                "a part directory survived publication"
            );
        }
        // Nothing left in the staging area either.
        assert!(
            !cache.path().join("tmp").join(&hash).exists(),
            "the temp directory was published, not left behind"
        );

        // And the part that cannot be checked any other way: a real playback,
        // computing the recipe from its own inputs, finds it.
        let info = mgr
            .start(file_id, 240, 0.0, None, None, "paul", "pb-after-produce")
            .await
            .expect("start");
        assert!(
            info.vod,
            "the producer and the player disagree about what this transcode is called"
        );
        assert_eq!(info.encoder, "cached");
        assert!(mgr.playlist(&info.session_id).await.is_some());
        assert!(mgr.stop_session(&info.session_id, "test").await);

        // Producing it again is a no-op rather than a second encode.
        assert!(
            mgr.produce(&file, 240, Instant::now() + Duration::from_secs(120))
                .await
                .expect("produce again")
                .is_none(),
            "an entry that already exists was produced a second time"
        );
    }

    /// An unfinished run keeps its claim and its bytes — and is never
    /// serveable.
    ///
    /// Those are the same fact from two sides, and the second is what makes the
    /// first safe. The claim is a *bookmark*: a two-hour 4K film on a contended
    /// box takes several passes, and a run that discarded its work each time it
    /// was interrupted would never finish one. Nothing can serve it in the
    /// meantime because a claim is not a hit, and if the node dies for good the
    /// stale-claim sweep takes the claim and its staging together.
    #[tokio::test]
    async fn an_unfinished_run_keeps_its_place_but_is_never_serveable() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        // The budget below is squeezed between two facts about the encoder,
        // and the two ends fail on opposite hardware:
        //
        //   too generous → a quick box finishes the whole film inside it, and
        //     "an unfinished run must not publish" fails on a run that finished
        //   too tight    → a busy box has not published a segment yet, and
        //     "the encoded part was thrown away" fails on a part never written
        //
        // Unpaced, both ends are facts about the CPU, and no constant satisfies
        // both: 700 ms failed the first way on a 16-core desktop, 100 ms failed
        // the second way on a 2-core runner, and 1200 ms failed the second way
        // again on the desktop once the suite around it got busier.
        //
        // Pacing the input fixes the upper end **arithmetically**: at READRATE,
        // a budget of B reaches B x READRATE seconds of source and no more, on
        // any hardware, so "it cannot have finished" stops being a hope. See
        // the pacing note in `a_preempted_producer_resumes_without_losing_picture`.
        //
        // The lower end cannot be closed the same way, because "ffmpeg starts
        // and publishes one segment" is genuinely a fact about the machine and
        // about what else is running on it. So it is not asserted on the first
        // try: a pass that produced nothing gets one more with twice the
        // budget, and only a second empty pass is a failure. That costs a slow
        // build a few seconds and costs a correct one nothing, where the
        // alternative — a bigger constant — costs every build every time and
        // still guesses.
        const SECONDS: u32 = 120;
        const READRATE: f64 = 5.0;
        // 25 s of a 120 s film on the first try and 51 s on the second, so
        // even the doubled budget is nowhere near the end; and 5 s for ffmpeg
        // to start against the 0.4 s of reading one segment needs.
        const PARTIAL_BUDGET: Duration = Duration::from_millis(5_000);

        if !crate::ffmpeg::pacing_caps().await.readrate {
            eprintln!(
                "SKIP: an_unfinished_run_keeps_its_place_but_is_never_serveable: \
                 `{}` has no -readrate (needs ffmpeg 5.1+), so the partial budget \
                 cannot be made independent of this machine's speed",
                crate::ffmpeg::ffmpeg_bin()
            );
            return;
        }

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, SECONDS);
        let file_id = seed_real_file(&store, &source).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let mgr = mgr.with_producer_tuning(ProducerTuning {
            pacing: Pacing {
                readrate: Some(READRATE),
                initial_burst: None,
                legacy_re: false,
            },
            ..ProducerTuning::default()
        });
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 240).await;

        // A short budget: it encodes some of the film and runs out of time.
        let staging = crate::cachekeep::staging_dir(cache.path(), &hash);
        let mut budget = PARTIAL_BUDGET;
        for attempt in 1..=2 {
            let started = Instant::now();
            let unfinished = mgr
                .produce(&file, 240, Instant::now() + budget)
                .await
                .expect("produce");
            assert!(
                unfinished.is_none(),
                "an unfinished run must not publish — but this box finished the \
                 whole {SECONDS}s fixture in {:?}, inside a {budget:?} budget \
                 that at {READRATE}x realtime reaches only {}s of it, so the \
                 pacing is not being applied rather than the cache being wrong",
                started.elapsed(),
                budget.as_secs_f64() * READRATE
            );
            if staging.join(crate::produce::part_dir(0)).exists() {
                break;
            }
            // Nothing was encoded, so there is nothing for the rest of this
            // test to be about. That is the lower end giving way — see above.
            assert!(
                attempt < 2,
                "two passes, {:?} and {:?}, and ffmpeg never published a \
                 segment: at {READRATE}x realtime one {}s segment is {}s of \
                 reading, so this box needed more than {budget:?} just to \
                 start an encoder",
                PARTIAL_BUDGET,
                budget,
                plurx_core::transcode::SEGMENT_SECONDS,
                f64::from(plurx_core::transcode::SEGMENT_SECONDS) / READRATE
            );
            budget *= 2;
        }

        assert!(
            store.cache_hit(&hash, NODE).await.expect("hit").is_none(),
            "an unfinished run must never be serveable"
        );
        assert!(
            !cache.path().join(&hash[..2]).join(&hash).exists(),
            "…and must not have a published directory"
        );
        // The bookmark, and the work it refers to.
        let claims = store
            .stale_cache_claims(NODE, i64::MAX)
            .await
            .expect("claims");
        assert_eq!(claims.len(), 1, "the claim that lets a later pass resume");
        assert!(
            staging.join(crate::produce::part_dir(0)).exists(),
            "the encoded part was thrown away, so the next pass starts from zero"
        );

        // And picking it up finishes the job from where it stopped rather than
        // from the beginning.
        //
        // Flat out, deliberately: the pacing above exists to bound what the
        // *budgeted* pass could reach, and this pass has no budget to bound.
        // Pacing it too would add SECONDS/READRATE seconds to every run of
        // this test in exchange for nothing.
        let mgr = mgr.with_producer_tuning(ProducerTuning::default());
        let made = mgr
            .produce(&file, 240, Instant::now() + Duration::from_secs(180))
            .await
            .expect("produce")
            .expect("the second pass finishes it");
        assert_eq!(made.recipe, hash);
        assert!(
            made.parts >= 2,
            "the second pass restarted from zero instead of resuming"
        );
        assert!(
            (made.duration_ms - (SECONDS as i64) * 1_000).abs() <= 1_000,
            "resuming across passes lost picture: {}ms of a {SECONDS}s source",
            made.duration_ms
        );
        assert!(
            store.cache_hit(&hash, NODE).await.expect("hit").is_some(),
            "and it is serveable now"
        );
        assert!(
            !staging.exists(),
            "the staging directory outlived the asset it built"
        );
    }

    /// Two producers, one recipe. The loser has to be told, because the
    /// alternative is not a wasted encode but a corrupted one: it would
    /// publish over the directory the winner is still writing into.
    #[tokio::test]
    async fn a_second_producer_stands_down_rather_than_racing() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let (mgr, _work, _cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 1080).await;

        // Somebody else is mid-encode.
        store
            .claim_cache_entry(&hash, file_id, 1, NODE, &format!("{}/{hash}", &hash[..2]))
            .await
            .expect("claim");

        assert!(
            mgr.produce(&file, 1080, Instant::now() + Duration::from_secs(30))
                .await
                .expect("produce")
                .is_none(),
            "the second producer started an encode against a claimed recipe"
        );
        // …and did not disturb the claim it lost to.
        let claims = store
            .stale_cache_claims(NODE, i64::MAX)
            .await
            .expect("claims");
        assert_eq!(
            claims.len(),
            1,
            "the winner's claim was removed by the loser"
        );
        assert_eq!(claims[0].relative_dir, format!("{}/{hash}", &hash[..2]));
    }

    #[tokio::test]
    async fn speculative_production_stands_down_while_offline_is_waiting() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let (mgr, _work, _cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");

        mgr.offline_waiting
            .store(true, std::sync::atomic::Ordering::Release);
        let result = mgr
            .produce(&file, 720, Instant::now() + Duration::from_secs(30))
            .await
            .expect("producer result");
        mgr.offline_waiting
            .store(false, std::sync::atomic::Ordering::Release);

        assert!(
            result.is_none(),
            "offline preparation must own the producer lane"
        );
        assert!(
            store
                .stale_cache_claims(NODE, i64::MAX)
                .await
                .expect("claims")
                .is_empty(),
            "standing down must happen before a recipe is claimed"
        );
    }

    #[tokio::test]
    async fn offline_preempts_speculation_and_resumes_its_published_part() {
        super::require_ffmpeg();
        use plurx_core::domain::{NewOfflinePackage, OfflineCreateOutcome};
        use plurx_core::store::SqliteStore;

        const SECONDS: u32 = 30;
        const READRATE: f64 = 10.0;
        if !crate::ffmpeg::pacing_caps().await.readrate {
            eprintln!(
                "SKIP: offline_preempts_speculation_and_resumes_its_published_part: \
                 `{}` has no -readrate (needs ffmpeg 5.1+)",
                crate::ffmpeg::ffmpeg_bin()
            );
            return;
        }

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store.create_user("paul", "hash", true).await.expect("user");
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, SECONDS);
        let file_id = seed_real_file(&store, &source).await;
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let package_id = "offline-preemption";
        let requested = NewOfflinePackage {
            id: package_id.to_owned(),
            request_id: "offline-preemption-request".to_owned(),
            user_id: user.id,
            file_id,
            node_id: NODE.to_owned(),
            source_path: file.path.to_string_lossy().into_owned(),
            source_size: file.size,
            source_mtime: file.mtime,
            target_height: 240,
            output_width: Some(320),
            output_height: Some(240),
            audio_index: None,
            audio_offset_ms: 0,
            subtitle_index: None,
            subtitle_language: None,
            subtitle_mode: "none".to_owned(),
            estimated_bytes: 1_000_000,
            reserved_bytes: 1_100_000,
            expires_at: i64::MAX,
        };
        assert!(matches!(
            store
                .create_offline_package(&requested, 10, 10_000_000, 20_000_000)
                .await
                .expect("create package"),
            OfflineCreateOutcome::Created(_)
        ));
        assert_eq!(
            store
                .claim_next_offline_package(NODE)
                .await
                .expect("claim package")
                .expect("queued package")
                .id,
            package_id
        );

        let (mgr, _work, cache) = cached_manager(&store);
        let mgr = Arc::new(mgr.with_producer_tuning(ProducerTuning {
            pacing: Pacing {
                readrate: Some(READRATE),
                initial_burst: None,
                legacy_re: false,
            },
            retry: Duration::from_millis(250),
        }));
        let speculative = {
            let mgr = Arc::clone(&mgr);
            let file = file.clone();
            tokio::spawn(async move {
                mgr.produce(&file, 240, Instant::now() + Duration::from_secs(180))
                    .await
            })
        };
        wait_for_part_segment(cache.path(), 0).await;

        let outcome = mgr
            .ensure_offline(
                package_id,
                &file,
                &OfflineSpec {
                    target_height: 240,
                    audio_index: None,
                    subtitle: OfflineSubtitle::None,
                },
                Instant::now() + Duration::from_secs(180),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("offline preparation");
        assert!(
            speculative
                .await
                .expect("join")
                .expect("producer")
                .is_none(),
            "the speculative pass must yield rather than publish"
        );
        let made = match outcome {
            OfflineProduceOutcome::Ready(made) | OfflineProduceOutcome::Cached(made) => made,
            other => panic!("offline preparation did not finish: {other:?}"),
        };
        assert!(
            made.parts >= 2,
            "offline preparation restarted instead of resuming the preempted part"
        );
    }

    /// Preempted mid-encode, then resumed — and the film that comes out has no
    /// hole in it.
    ///
    /// This is the producer's whole reason for existing in parts, and the
    /// failure it guards against is the quietest one in the system: resuming a
    /// few hundred milliseconds late loses a moment of picture in the middle of
    /// a film, in a file nobody watches until next week, with every log line
    /// green. Only measuring the assembled timeline against the source catches
    /// it, so that is what this does.
    ///
    /// Everything about the timing here is derived from the producer's own
    /// output or from a rate this test sets — see the comment on the pacing
    /// below. A test of preemption that sleeps a fixed interval is really a
    /// test of how fast the machine is.
    #[tokio::test]
    async fn a_preempted_producer_resumes_without_losing_picture() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        // The producer's input is paced for this test, which is what makes it
        // a test rather than a race.
        //
        // Preemption is only observable while the encoder is still running, so
        // the previous shape — a 240-second fixture and a 400 ms sleep — was
        // an unwritten assertion that this machine needs more than 400 ms to
        // transcode 240 seconds of 160x120. A 2-core CI box needs about five
        // seconds and passed; a 16-core desktop needs about a third of one,
        // finished before the first interrupt, and failed with "nothing was
        // actually preempted". No fixture length satisfies both: whatever is
        // long enough for the fast box is minutes of CI time on the slow one.
        //
        // `-readrate N` removes the CPU from the equation. The encoder reads
        // its input at N times realtime, so a part's wall-clock duration is
        // SECONDS / READRATE on any box quick enough to keep up — and the
        // margin for "quick enough" is what the numbers below are chosen for:
        // the slowest machine seen here manages about 50x realtime on this
        // fixture, five times the rate asked for. A box slower than READRATE
        // just takes longer and still gets preempted, so the failure mode is
        // a slow test rather than a false one.
        const SECONDS: u32 = 30;
        const READRATE: f64 = 10.0;
        // Long enough that a running encoder cannot miss it (PRODUCER_POLL is
        // a quarter-second), short enough not to dominate the test.
        const HOLD: Duration = Duration::from_millis(750);

        // `-readrate` landed in ffmpeg 5.1 and is a hard exit on anything
        // older, so an ancient build gets a skip rather than a failure — the
        // same bargain `pipeprobe` strikes with a missing zscale. CI has a
        // modern ffmpeg, so the coverage is not lost.
        if !crate::ffmpeg::pacing_caps().await.readrate {
            eprintln!(
                "SKIP: a_preempted_producer_resumes_without_losing_picture: \
                 `{}` has no -readrate (needs ffmpeg 5.1+), so a producer part's \
                 duration cannot be made independent of this machine's speed",
                crate::ffmpeg::ffmpeg_bin()
            );
            return;
        }

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, SECONDS);
        let file_id = seed_real_file(&store, &source).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let mgr = mgr.with_producer_tuning(ProducerTuning {
            pacing: Pacing {
                readrate: Some(READRATE),
                // No burst: a burst is exactly the "read the first N seconds
                // flat out" behaviour this test needs not to have.
                initial_burst: None,
                legacy_re: false,
            },
            // Production waits five seconds before asking for the hardware
            // again, and that is the right number for a box where a viewer
            // just took the slot. Here it is dead time the test pays once per
            // preemption, and the retry is not what is under test.
            retry: Duration::from_millis(250),
        });
        let mgr = Arc::new(mgr);
        let file = store.get_file(file_id).await.expect("get").expect("file");

        let run = {
            let mgr = Arc::clone(&mgr);
            let file = file.clone();
            tokio::spawn(async move {
                mgr.produce(&file, 240, Instant::now() + Duration::from_secs(180))
                    .await
            })
        };
        // Interrupt it twice, so the asset is assembled from three parts and a
        // per-join error compounds rather than cancels.
        //
        // Each interrupt waits for the part it is about to kill to have
        // published a segment, and then for the *next* part to publish one
        // before interrupting again. Both waits are observations of the
        // producer's own output, not sleeps: they are what stops an interrupt
        // landing before the encoder started (killing a part that produced
        // nothing, which the producer renumbers away) or during the retry
        // backoff (where it is simply lost — the reason the old loop's second
        // interrupt never landed, so this test only ever built two parts even
        // where it passed).
        for part in 0..2usize {
            wait_for_part_segment(cache.path(), part).await;
            let queued = mgr.admissions.wait_for_slot();
            tokio::time::sleep(HOLD).await;
            drop(queued);
            wait_for_part_segment(cache.path(), part + 1).await;
        }
        let made = run
            .await
            .expect("join")
            .expect("produce")
            .expect("something was produced");
        // The assertion that stops this test passing for the wrong reason: an
        // encode that finished before it was interrupted would satisfy every
        // check below while exercising none of the resume path.
        assert!(
            made.parts >= 3,
            "assembled from {} part(s) — two interrupts must leave three, and \
             fewer means the encode was not actually preempted, so this test \
             proved nothing about resuming",
            made.parts
        );
        let hash = made.recipe;

        let dir = cache.path().join(&hash[..2]).join(&hash);
        let playlist = tokio::fs::read_to_string(dir.join("index.m3u8"))
            .await
            .expect("playlist");
        let part = crate::produce::Part::from_playlist(&playlist);

        // Continuous numbering, every segment on disk and non-empty.
        for (i, name) in part.segments.iter().enumerate() {
            assert_eq!(*name, format!("seg{i:05}.ts"), "a gap at {i}: {playlist}");
            let meta = tokio::fs::metadata(dir.join(name)).await.expect(name);
            assert!(meta.len() > 0, "{name} is empty");
        }

        // And the timeline covers the source. A resume that restarted a beat
        // late would land short here, by exactly the picture it dropped.
        let produced_ms = part.duration_ms();
        let source_ms = (SECONDS as i64) * 1000;
        // Half a segment. The resume works in whole published segments, so a
        // mistake here shows up as a segment lost or repeated — two seconds —
        // and a tolerance loose enough to swallow that would hide the only
        // thing this test exists to find. Measured drift on this fixture is
        // zero.
        assert!(
            (produced_ms - source_ms).abs() <= 1_000,
            "assembled {produced_ms}ms from a {source_ms}ms source — \
             a resume lost or repeated picture:\n{playlist}"
        );
        // Belt and braces: ffprobe the assembled asset, because a playlist can
        // claim a duration its bytes do not have.
        let probed = std::process::Command::new(
            std::env::var("PLURX_FFPROBE").unwrap_or_else(|_| "ffprobe".into()),
        )
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            "-allowed_extensions",
            "ALL",
        ])
        .arg(dir.join("index.m3u8"))
        .output()
        .expect("ffprobe");
        let probed_s: f64 = String::from_utf8_lossy(&probed.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0);
        assert!(
            (probed_s - SECONDS as f64).abs() <= 2.0,
            "ffprobe reads {probed_s}s from a {SECONDS}s source"
        );

        // It serves, which is the only thing any of this was for.
        let info = mgr
            .start(file_id, 240, 0.0, None, None, "paul", "pb-resumed")
            .await
            .expect("start");
        assert!(info.vod, "a resumed asset is not findable");
        assert!(mgr.stop_session(&info.session_id, "test").await);
    }

    /// Wait until part `index` of the one staged pre-transcode has published a
    /// segment.
    ///
    /// The producer publishes each part's segments as it makes them, so this
    /// is the encoder saying "I am running and I got somewhere" in the only
    /// vocabulary it has. Polling for it replaces a sleep, and a sleep here is
    /// always a guess about CPU speed dressed up as a constant.
    async fn wait_for_part_segment(cache: &std::path::Path, index: usize) {
        let staging = cache.join(crate::cachekeep::STAGING);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            // One recipe is in flight, but find it rather than recomputing the
            // hash: a hash spelled out twice is a hash that can drift.
            if let Ok(mut entries) = tokio::fs::read_dir(&staging).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let dir = entry.path().join(crate::produce::part_dir(index));
                    if !read_part(&dir).await.is_empty() {
                        return;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "part {index} never published a segment under {}",
                staging.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A file whose real details are on disk, so ffmpeg can actually read it.
    async fn seed_real_file(store: &Arc<dyn Store>, path: &std::path::Path) -> i64 {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = store
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
            .expect("movie");
        let meta = std::fs::metadata(path).expect("fixture");
        store
            .upsert_file(
                movie,
                &path.to_string_lossy(),
                meta.len() as i64,
                1,
                &ProbeResult {
                    duration_ms: Some(6_000),
                    container: Some("mkv".into()),
                    video_codec: Some("h264".into()),
                    width: Some(160),
                    height: Some(120),
                    ..Default::default()
                },
            )
            .await
            .expect("file")
    }

    /// A node with no cache root is not a broken node — it is the ordinary
    /// case, and every path has to read as a plain miss.
    #[tokio::test]
    async fn a_manager_without_a_cache_root_simply_always_misses() {
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        assert!(mgr.digest().is_none());
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let encoder = mgr.encoder().await;
        let opts = mgr.options_for(encoder, &file, 1080, 0.0, None, None, None);
        assert!(mgr
            .serve_cached(&file, &opts, encoder, "Heat", "paul", "pb-1")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn manager_reads_prefs_and_runs_session_lifecycle() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );

        // Defaults with an empty store.
        let prefs = mgr.lang_prefs().await;
        assert_eq!(prefs.audio_lang, "eng");
        // No hardware caps → software encoder.
        assert_eq!(mgr.encoder().await, Encoder::Software);
        assert_eq!(mgr.active_sessions().await, 0);
        assert!(mgr.list_deliveries().await.is_empty());

        // Settings feed the language prefs.
        store.put_setting(keys::AUDIO_LANG, "jpn").await.expect("s");
        store.put_setting(keys::SUB_LANG, "eng").await.expect("s");
        store
            .put_setting(keys::SUB_MODE, "always")
            .await
            .expect("s");
        let prefs = mgr.lang_prefs().await;
        assert_eq!(prefs.audio_lang, "jpn");

        // Unknown session lookups fail fast (no waiting).
        assert!(mgr.playlist("missing").await.is_none());
        assert!(mgr.segment("missing", "seg00000.ts").await.is_none());
        assert!(mgr.segment("missing", "../evil").await.is_none());
        assert!(!mgr.stop_session("missing", "test").await);

        // A real start spawns ffmpeg (it fails async on the fake path, but the
        // session is created and tracked). Then the admin stop kills it.
        let info = mgr
            .start(file_id, 720, 0.0, None, None, "paul", "pb-paul")
            .await
            .expect("start");
        assert_eq!(info.encoder, "software (x264)");
        assert_eq!(mgr.active_sessions().await, 1);
        let sessions = mgr.list_deliveries().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0.user_name, "paul");
        assert_eq!(sessions[0].1, crate::delivery::Method::Transcode);
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert_eq!(mgr.active_sessions().await, 0);

        // The copy-video path likewise creates and tears down a session.
        let info = mgr
            .start_copy(
                file_id,
                5.0,
                Some(1),
                CopySessionOptions {
                    transcode_audio: true,
                    preserve_dolby_vision: false,
                },
                "paul",
                "pb-paul",
            )
            .await
            .expect("start_copy");
        assert_eq!(info.encoder, "copy");
        // The two HLS kinds share a struct and are told apart structurally,
        // never by the encoder label: that label goes to "cached" on a cache
        // hit and is rewritten by the hardware→software fallback, either of
        // which would have a copy-remux reporting itself as a transcode.
        let copies = mgr.list_deliveries().await;
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].1, crate::delivery::Method::HlsCopy);
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert!(
            mgr.list_deliveries().await.is_empty(),
            "and the row goes with the session"
        );
    }

    /// Seeking must not leave the old session running. Before this, every seek
    /// stacked another ffmpeg for up to ~75s (idle timeout + reaper tick).
    #[tokio::test]
    async fn a_new_session_supersedes_the_same_players_old_one() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        // Two players deliberately coexist below; this test is about the
        // supersession key, not the CPU pool, so give the pool room — on a
        // 2-core runner the machine-derived budget would refuse the second
        // player and fail the test for the wrong reason.
        store
            .put_setting(keys::SW_POOL_THREADS, "64")
            .await
            .expect("pool headroom");

        // Play, then "seek" three times. Only the newest survives.
        let first = mgr
            .start(file_id, 720, 0.0, None, None, "paul", "pb-paul")
            .await
            .expect("start");
        let second = mgr
            .start(file_id, 720, 600.0, None, None, "paul", "pb-paul")
            .await
            .expect("seek");
        let third = mgr
            .start(file_id, 720, 1200.0, None, None, "paul", "pb-paul")
            .await
            .expect("seek again");
        assert_eq!(mgr.active_sessions().await, 1);
        assert!(mgr.playlist(&first.session_id).await.is_none());
        assert!(!mgr.stop_session(&second.session_id, "test").await);
        assert!(mgr.stop_session(&third.session_id, "test").await);

        // The copy path supersedes too, and across paths: a transcode fallback
        // after a copy attempt must not leave the copy remux reading the disk.
        let copy = mgr
            .start_copy(
                file_id,
                0.0,
                None,
                CopySessionOptions {
                    transcode_audio: false,
                    preserve_dolby_vision: false,
                },
                "paul",
                "pb-paul",
            )
            .await
            .expect("copy");
        let fallback = mgr
            .start(file_id, 720, 0.0, None, None, "paul", "pb-paul")
            .await
            .expect("fallback");
        assert_eq!(mgr.active_sessions().await, 1);
        assert!(!mgr.stop_session(&copy.session_id, "test").await);

        // Another player is untouched — and this is the part that changed.
        // Supersession used to be keyed by (viewer, file), so one account
        // watching the same film in two places meant each device killed the
        // other's stream on every seek. Two player instances now coexist even
        // as the same person, on the same file, from the same account.
        let laptop = mgr
            .start(file_id, 720, 0.0, None, None, "paul", "pb-laptop")
            .await
            .expect("second device");
        assert_eq!(mgr.active_sessions().await, 2);
        let reseek = mgr
            .start(file_id, 720, 30.0, None, None, "paul", "pb-paul")
            .await
            .expect("the first player seeks");
        assert_eq!(
            mgr.active_sessions().await,
            2,
            "one player's seek must not touch another player's stream"
        );
        assert!(!mgr.stop_session(&fallback.session_id, "test").await);
        assert!(mgr.stop_session(&laptop.session_id, "test").await);
        assert!(mgr.stop_session(&reseek.session_id, "test").await);
        assert_eq!(mgr.active_sessions().await, 0);
    }

    /// Creating a session spawns a process and kills its predecessor, so a
    /// replayed create is not harmless — it orphans an encoder. The
    /// idempotency key makes the retry return what the first call built.
    #[tokio::test]
    async fn a_repeated_create_returns_the_same_session() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        let request = SessionRequest {
            file_id,
            playback_id: "pb-1".into(),
            request_id: Some("req-1".into()),
            kind: SessionKind::Transcode { height: 720 },
            start_seconds: 0.0,
            audio_index: None,
            subtitle_burn: None,
            audio_offset_ms: 0,
        };

        let shifted = SessionRequest {
            audio_offset_ms: 250,
            ..request.clone()
        };
        assert_ne!(
            shifted.fingerprint(),
            request.fingerprint(),
            "session-scoped audio sync must identify different output bytes"
        );

        let first = mgr.create_session(&request, "paul").await.expect("create");
        let again = mgr.create_session(&request, "paul").await.expect("replay");
        assert_eq!(
            first.session_id, again.session_id,
            "a replayed create must not spawn a second encoder"
        );
        assert_eq!(mgr.active_sessions().await, 1);
        assert_eq!(again.start_seconds, first.start_seconds);

        // The same key asking for something else is a mistake worth naming,
        // not a quiet second stream.
        let different = SessionRequest {
            start_seconds: 600.0,
            ..request.clone()
        };
        assert!(mgr
            .create_session(&different, "paul")
            .await
            .is_err_and(|e| e.contains("already used")));
        assert_eq!(mgr.active_sessions().await, 1);

        // A fresh key from the same player supersedes, as any restart does.
        let next = SessionRequest {
            request_id: Some("req-2".into()),
            start_seconds: 600.0,
            ..request.clone()
        };
        let moved = mgr.create_session(&next, "paul").await.expect("seek");
        assert_ne!(moved.session_id, first.session_id);
        assert_eq!(mgr.active_sessions().await, 1);

        // And once a session is gone, its key is stale rather than binding —
        // otherwise a client that reused an id would be told "conflict"
        // forever.
        assert!(mgr.stop_session(&moved.session_id, "test").await);
        let revived = mgr.create_session(&next, "paul").await.expect("recreate");
        assert_ne!(revived.session_id, moved.session_id);
        assert!(mgr.stop_session(&revived.session_id, "test").await);
    }

    /// The check-then-act race the reservation exists to close: two creates
    /// carrying the same request id, in flight at the same time, must produce
    /// one session between them — not one encoder each with the loser handed
    /// a stream its twin's supersession already killed. No interleaving is
    /// asserted, only the outcome, so this passes whether the runtime overlaps
    /// them or happens to serialize them.
    #[tokio::test]
    async fn concurrent_creates_with_one_request_id_share_one_session() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        let request = SessionRequest {
            file_id,
            playback_id: "pb-race".into(),
            request_id: Some("req-race".into()),
            kind: SessionKind::Transcode { height: 720 },
            start_seconds: 0.0,
            audio_index: None,
            subtitle_burn: None,
            audio_offset_ms: 0,
        };

        let (a, b) = tokio::join!(
            mgr.create_session(&request, "paul"),
            mgr.create_session(&request, "paul"),
        );
        let a = a.expect("first create");
        let b = b.expect("second create");
        assert_eq!(
            a.session_id, b.session_id,
            "both callers must be handed the one session the id names"
        );
        assert_eq!(mgr.active_sessions().await, 1, "and exactly one exists");
        assert!(mgr.stop_session(&a.session_id, "test").await);
    }

    /// A create that fails must clear its reservation on the way out. The
    /// second call reuses the id for a *different* stream on purpose: were the
    /// failed reservation left behind, this would be refused as a conflict —
    /// a client whose first attempt died would find its id poisoned and could
    /// never retry.
    #[tokio::test]
    async fn a_failed_create_does_not_poison_its_request_id() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
            Pipeline::Cpu,
        );
        let request = SessionRequest {
            file_id: 999_999, // nothing has this id, so the create fails
            playback_id: "pb-fail".into(),
            request_id: Some("req-fail".into()),
            kind: SessionKind::Transcode { height: 720 },
            start_seconds: 0.0,
            audio_index: None,
            subtitle_burn: None,
            audio_offset_ms: 0,
        };
        assert!(mgr.create_session(&request, "paul").await.is_err());

        let retry = SessionRequest {
            file_id,
            ..request.clone()
        };
        let info = mgr
            .create_session(&retry, "paul")
            .await
            .expect("a failed attempt must not bind its request id");
        assert!(mgr.stop_session(&info.session_id, "test").await);
    }

    #[tokio::test]
    async fn producing_requires_a_listed_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No playlist yet.
        assert!(!session_producing(dir.path()).await);
        // Header only, no segment listed (ffmpeg has started but nothing finished).
        tokio::fs::write(
            dir.path().join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n",
        )
        .await
        .expect("write playlist");
        assert!(!session_producing(dir.path()).await);
        // A bare `seg*.ts` FILE existing must NOT count (the old bug) — only a
        // playlist entry does.
        tokio::fs::write(dir.path().join("seg00000.ts.tmp"), b"partial")
            .await
            .expect("write temp seg");
        assert!(!session_producing(dir.path()).await);
        // A finished, listed segment: producing.
        tokio::fs::write(
            dir.path().join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.000,\nseg00000.ts\n",
        )
        .await
        .expect("write playlist with segment");
        assert!(session_producing(dir.path()).await);
    }
}
