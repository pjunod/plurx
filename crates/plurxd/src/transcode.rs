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

use plurx_core::store::{keys, Store};
use plurx_core::transcode::{
    self, Encoder, EncoderCaps, Pacing, Pipeline, PipelineDigest, Recipe, ToneMap, TranscodeOptions,
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
/// forward buffer ahead of what it is showing: with a 60-second buffer the
/// frontier sits a minute past the picture on screen. Pruning at a fixed
/// distance behind the frontier therefore deletes media the viewer is about to
/// watch — or is watching. Retention covers the forward buffer, the back
/// buffer the client keeps for scrubbing, and an allowance for a retry or a
/// playlist reload landing on something older.
const CLIENT_FORWARD_BUFFER_SECS: i64 = 60;
const CLIENT_BACK_BUFFER_SECS: i64 = 30;
const RETRY_ALLOWANCE_SECS: i64 = 30;
const RETENTION_SECS: i64 =
    CLIENT_FORWARD_BUFFER_SECS + CLIENT_BACK_BUFFER_SECS + RETRY_ALLOWANCE_SECS;
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

    /// Bytes of published segments lying entirely after `ms`.
    fn bytes_after_ms(&self, ms: i64) -> i64 {
        self.segs
            .iter()
            .filter(|s| s.start_ms >= ms)
            .map(|s| s.bytes)
            .sum()
    }

    /// Segments old enough to delete: those that END before the retention
    /// window opens. A segment straddling the boundary is kept — half a
    /// segment is no use to anyone and the arithmetic is cheap.
    fn prunable(&self, keep_from_ms: i64) -> impl Iterator<Item = &SegmentMeta> {
        self.segs
            .iter()
            .filter(move |s| s.bytes > 0 && s.end_ms <= keep_from_ms)
    }
}

/// Parse `EXTINF` durations and segment URIs out of an HLS media playlist.
///
/// Pure, and the reason the copy path's variable segment lengths stop being a
/// guess: the playlist is the only place the true duration of a copied segment
/// is written down.
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
            pending_ms = rest
                .split(',')
                .next()
                .and_then(|d| d.trim().parse::<f64>().ok())
                .filter(|d| *d >= 0.0)
                .map(|secs| (secs * 1000.0).round() as i64);
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
        });
        cursor_ms += duration_ms;
    }
    out
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

/// Whether a session should be held, given how far ahead it is, how much
/// scratch every session is using between them, and whether it is already
/// held.
///
/// Any single limit is enough to suspend; resuming needs all of them below
/// half. The asymmetry is deliberate: a single threshold makes a session
/// sitting on a boundary toggle on every evaluation, which is a stream of
/// signals and log lines to accomplish nothing. Resuming early is the safe
/// direction — the cost of being wrong is some disk, and the cost of the other
/// error is a viewer who runs dry.
fn should_suspend(
    ahead: Ahead,
    global_bytes: i64,
    limits: AheadLimits,
    currently_suspended: bool,
) -> bool {
    let divisor = if currently_suspended { 2 } else { 1 };
    let over = |value: i64, limit: i64| limit > 0 && value > limit / divisor;
    over(ahead.seconds, limits.max_secs)
        || over(ahead.bytes, limits.max_bytes)
        || over(global_bytes, limits.global_max_bytes)
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
) -> Result<Child, String> {
    // `-progress pipe:1` is a global option, so it can lead the vector; the
    // HLS muxer writes to files, which leaves stdout free to carry it.
    let mut full: Vec<String> = vec!["-progress".into(), "pipe:1".into()];
    full.extend_from_slice(args);
    let mut child = tokio::process::Command::new(ffmpeg_bin())
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
) -> Result<(Child, tokio::process::ChildStdout), String> {
    let mut full: Vec<String> = vec!["-progress".into(), "pipe:2".into()];
    full.extend_from_slice(args);
    let mut child = tokio::process::Command::new(ffmpeg_bin())
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

/// Fail a session that has stopped producing, and only then.
///
/// This replaced a single verdict taken at a fixed deadline ("no segment after
/// 30s ⇒ dead"), which could not tell a wedged pipeline from a slow one and
/// killed both. It answers a different question — *is output still moving?* —
/// on a poll, which is strictly more informative in both directions: a session
/// that never opened its input still fails at the same deadline, a session
/// decoding 4K at 0.4x is left alone, and a session that produced a few
/// seconds and then wedged is now caught too (the old check looked once and,
/// having found a segment, never looked again).
///
/// Exits as soon as there is real output, when the child dies on its own
/// (which the playlist and segment readers already report), or when the
/// session is failed by someone else.
async fn watch_for_stall(session: Arc<Session>, dir: PathBuf, sid: String) {
    // A generous floor before the first verdict: opening a 4K file over NFS
    // and filling a first segment is legitimately slow, and `stalled_for`
    // counts from session start, so judging earlier would fail cold opens.
    tokio::time::sleep(SOFTWARE_GRACE).await;
    loop {
        if session.failed.load(Relaxed) || session_producing(&dir).await {
            return;
        }
        let exited = {
            let mut child = session.child.lock().await;
            child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
        };
        if exited {
            return;
        }
        // Stopped on purpose, or still moving: neither is a stall.
        if !session.suspended.load(Relaxed) && session.progress.stalled_for() >= PROGRESS_STALL {
            tracing::error!(
                session = %sid,
                stalled_s = session.progress.stalled_for().as_secs(),
                produced_ms = session.progress.out_time_ms(),
                "transcode produced no playable segment and its output has stopped advancing; \
                 failing the session — the source is likely undecodable by this ffmpeg build \
                 (e.g. a Dolby Vision profile it can't handle)"
            );
            session.kill_child().await;
            session.failed.store(true, Relaxed);
            return;
        }
        tokio::time::sleep(WATCHDOG_POLL).await;
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
    last_access: Mutex<Instant>,
    // -- metadata for the activity page --
    file_id: i64,
    item_id: i64,
    item_title: String,
    user_name: String,
    /// The player instance that owns this session — the supersession key.
    playback_id: String,
    /// Where this session's timeline begins in the source, so a recovered
    /// (idempotent) create reports the same offset the first one did.
    start_seconds: f64,
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
    /// This session's share of scratch, cached from the last refresh so the
    /// global budget can be summed without re-reading every session's disk.
    ahead_bytes: AtomicI64,
    /// Live encode telemetry (see [`Progress`]).
    progress: Arc<Progress>,
    /// What kind of work this is, for the admission record (see
    /// [`crate::admission::Workload::class`]). Kept on the session because the
    /// speed that matters is measured while it runs, long after the file that
    /// described it went out of scope.
    class: String,
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
    /// Bytes of segment actually handed to this client, and how fast.
    ///
    /// The player cannot measure this for itself on every transport: native
    /// HLS (Safari's HEVC path) exposes no such number, and hls.js's estimator
    /// exists only when hls.js is the one fetching. The server serves every
    /// segment on every path, so it is the one place the answer always exists.
    delivery: Meter,
    /// True while the child is SIGSTOPped for running too far ahead of the
    /// playhead. Everything that judges a session's health has to know: a
    /// suspended encoder makes no progress *on purpose*.
    suspended: AtomicBool,
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

    /// Re-read the playlist and re-measure what is on disk.
    ///
    /// Sizes already known are carried forward by index, so a long session
    /// stats only the segments that appeared since the last refresh rather
    /// than the whole directory each time.
    async fn refresh_segments(&self) {
        let Ok(raw) = tokio::fs::read(self.dir.join("index.m3u8")).await else {
            return;
        };
        let mut segs = parse_playlist(&String::from_utf8_lossy(&raw));
        {
            let known = self.segments.lock().await;
            let mut sizes: HashMap<i64, i64> = HashMap::with_capacity(known.segs.len());
            for s in known.segs.iter().filter(|s| s.bytes > 0) {
                sizes.insert(s.index, s.bytes);
            }
            for s in segs.iter_mut() {
                if let Some(b) = sizes.get(&s.index) {
                    s.bytes = *b;
                }
            }
        }
        for s in segs.iter_mut().filter(|s| s.bytes == 0) {
            if let Ok(meta) = tokio::fs::metadata(self.dir.join(&s.name)).await {
                s.bytes = meta.len() as i64;
            }
        }
        let index = SegmentIndex { segs };
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
        }
        *self.segments.lock().await = index;
    }

    async fn ahead(&self) -> Option<Ahead> {
        ahead_of(
            &*self.segments.lock().await,
            self.fetched_end_ms.load(Relaxed).max(0),
        )
    }
}

/// One live session as the activity page and the stats overlay see it.
async fn session_info(id: &str, s: &Session) -> SessionInfo {
    let ahead = s.ahead().await;
    SessionInfo {
        id: id.to_owned(),
        file_id: s.file_id,
        item_id: s.item_id,
        item_title: s.item_title.clone(),
        user_name: s.user_name.clone(),
        target_height: s.target_height,
        encoder: *s.encoder_label.lock().await,
        started_unix: s.started_unix,
        idle_seconds: s.last_access.lock().await.elapsed().as_secs(),
        speed: s.progress.speed(),
        recent_speed: s.progress.recent_speed(),
        out_time_ms: s.progress.out_time_ms(),
        ahead_seconds: ahead.map(|a| a.seconds),
        ahead_bytes: ahead.map(|a| a.bytes),
        delivered_bytes: s.delivery.total_bytes(),
        delivered_bps: s.delivery.recent_bps().map(|b| b * 8),
        delivered_idle_ms: s.delivery.idle_for_ms(),
        suspended: s.suspended.load(Relaxed),
    }
}

/// A segment, open and ready to stream.
pub struct SegmentFile {
    pub file: tokio::fs::File,
    pub len: u64,
}

pub struct StartInfo {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
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
    pub suspended: bool,
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionKind {
    Transcode {
        height: i64,
    },
    /// Copy the source video; `aac` re-encodes the audio the client can't take.
    Copy {
        aac: bool,
    },
}

impl SessionRequest {
    /// A stable description of the output this request would produce. Start
    /// position is included: two creates at different offsets are different
    /// streams, and answering the second with the first would silently seek
    /// the viewer somewhere they didn't ask to be.
    fn fingerprint(&self) -> String {
        let kind = match self.kind {
            SessionKind::Transcode { height } => format!("t{height}"),
            SessionKind::Copy { aac } => format!("c{}", u8::from(aac)),
        };
        format!(
            "{}:{kind}:{:.3}:{}:{}",
            self.file_id,
            self.start_seconds,
            self.audio_index.unwrap_or(-1),
            self.subtitle_burn.unwrap_or(-1)
        )
    }
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

pub struct TranscodeManager {
    store: Arc<dyn Store>,
    work_dir: PathBuf,
    caps: EncoderCaps,
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
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Answered creation requests: `request_id` -> (session id, fingerprint).
    /// Small by construction — an entry is dropped as soon as its session is
    /// gone, and one player instance has one in flight at a time.
    requests: Mutex<HashMap<String, (String, String)>>,
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
        TranscodeManager {
            store,
            work_dir,
            caps,
            pipeline,
            admissions: Admissions::new(),
            cache: None,
            sessions: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
        }
    }

    /// Point the manager at a cache root, and tell it what this node's output
    /// is identified by.
    ///
    /// Separate from the constructor because both pieces come from elsewhere:
    /// the ffmpeg build from the startup probe, the node id from the store.
    /// Chaining keeps every existing call site — and every test that does not
    /// care about caching — unchanged.
    pub fn with_cache(mut self, cache_dir: PathBuf, ffmpeg_build: String, node_id: String) -> Self {
        self.cache = Some(CacheConfig {
            dir: cache_dir,
            ffmpeg_build,
            node_id,
        });
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

    /// This node's output identity, for naming a transcode.
    fn digest(&self) -> Option<PipelineDigest> {
        let cache = self.cache.as_ref()?;
        Some(PipelineDigest {
            ffmpeg_build: cache.ffmpeg_build.clone(),
            encoder: Encoder::Software, // replaced per lookup
            pipeline: self.pipeline,
        })
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
        // preferences otherwise. Burn the chosen text subtitle, since an HLS
        // transcode delivers a single flat stream.
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
    fn options_for(
        &self,
        encoder: Encoder,
        file: &plurx_core::domain::MediaFile,
        target_height: i64,
        start_seconds: f64,
        audio_index: Option<i64>,
        subtitle_burn: Option<plurx_core::transcode::SubtitleBurn>,
    ) -> TranscodeOptions {
        TranscodeOptions {
            target_height,
            video_bitrate_kbps: bitrate_for_height(target_height),
            audio_index,
            start_seconds,
            tone_map: tone_map_pref(),
            // The node proved a graph; this session may still not be entitled
            // to it (HLG, Dolby Vision, a burned subtitle of either kind, a
            // light source, an encoder it cannot feed). Deciding once, here,
            // is what keeps the log line honest — `pipeline=` is what actually
            // ran, not what the box is capable of.
            pipeline: Pipeline::for_session(
                self.pipeline,
                encoder,
                file.hdr.as_deref(),
                transcode::heavy_source(file),
                subtitle_burn.is_some(),
            ),
            subtitle_burn,
            // Only where the startup probe proved this build takes it. A
            // family that needs the flag and cannot have it still works; its
            // segments just follow the encoder's GOP, which is slower to start
            // and is logged as such at boot.
            force_idr: self.caps.forced_idr.wanted_by(encoder),
            ..Default::default()
        }
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
        digest.encoder = encoder;
        let hash = Recipe {
            digest: &digest,
            file,
            opts,
            audio_copied: false,
        }
        .hash();

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
            last_access: Mutex::new(Instant::now()),
            file_id: file.id,
            item_id: file.item_id,
            item_title: item_title.to_owned(),
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            start_seconds: 0.0,
            target_height: opts.target_height,
            encoder_label: Mutex::new("cached"),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            progress: Arc::new(Progress::new()),
            class: String::new(),
            hw_slot: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            suspended: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));
        tracing::info!(
            %session_id, recipe = %hash, file = file.id,
            "serving a cached transcode — no encoder started"
        );
        Some(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id,
            duration_ms: file.duration_ms,
            // A cached asset is the whole title, so playback starts at zero and
            // the player seeks into it. `start_seconds` on a live session
            // exists because the encoder had to be told where to begin; here
            // there is no encoder and nothing to tell.
            start_seconds: 0.0,
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
        let Some(cache) = self.cache.as_ref() else {
            return Ok(None);
        };
        let encoder = self.encoder().await;
        // Through the same track selection a real playback uses. Not an
        // optimisation — the tracks are part of the recipe, so producing with
        // "no audio track chosen" makes an entry named for a session that will
        // never be requested.
        let Tracks {
            audio_index,
            subtitle_burn,
        } = self.select_tracks(file, None, None).await;
        let opts = self.options_for(
            encoder,
            file,
            target_height,
            0.0,
            audio_index,
            subtitle_burn,
        );
        let mut digest = self.digest().ok_or("no cache digest")?;
        digest.encoder = encoder;
        let hash = Recipe {
            digest: &digest,
            file,
            opts: &opts,
            audio_copied: false,
        }
        .hash();

        // Already there. Not an error and not worth a log line — the candidate
        // list is a *prediction*, and predicting something that is already true
        // is the system working.
        if matches!(
            self.store.cache_hit(&hash, &cache.node_id).await,
            Ok(Some(_))
        ) {
            return Ok(None);
        }
        let relative = format!("{}/{hash}", &hash[..2]);
        // The claim is both a lock and a bookmark.
        //
        // A claim we did not take means somebody else owns this recipe — on
        // another node, or in another process — and standing down is the point:
        // two producers on one film is an hour of GPU spent twice, and the
        // loser would publish over the winner's directory.
        //
        // A claim we already hold on THIS node means an earlier pass ran out of
        // time part-way through. That is not somebody else; it is us, last
        // night. `JobManager::producing` allows one pass at a time here, so an
        // incomplete local claim cannot belong to a producer that is currently
        // running — it is a bookmark, and the work behind it is resumable.
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
            .map_err(|e| e.to_string())?;
        // Built beside the final directory so publication is a rename within
        // one filesystem — atomic, and the only way a half-written asset cannot
        // be observed as a whole one. Named for the recipe rather than a fresh
        // uuid, so the next pass can find it.
        let temp = crate::cachekeep::staging_dir(&cache.dir, &hash);
        if !taken {
            let resumable = tokio::fs::metadata(&temp).await.is_ok();
            if !resumable {
                tracing::debug!(
                    recipe = %hash, file = file.id,
                    "cache entry claimed elsewhere; standing down"
                );
                return Ok(None);
            }
            tracing::info!(
                recipe = %hash, file = file.id,
                "resuming a pre-transcode an earlier pass left unfinished"
            );
        }

        let outcome = self
            .produce_into(&temp, file, &opts, encoder, &hash, deadline)
            .await;
        let published = match outcome {
            Ok(Some(assembled)) => assembled,
            Ok(None) => {
                // Nothing publishable yet. The claim and the staged parts BOTH
                // stay, which is what makes this resumable: the next pass finds
                // the bookmark, reads what is already encoded and carries on
                // from that boundary rather than from zero. A two-hour 4K film
                // on a contended box may take several passes, and discarding
                // the work each time would mean it never finished at all.
                //
                // Nothing can serve it meanwhile — a claim is not a hit — and
                // if this node dies for good, the stale-claim sweep takes the
                // claim and its staging together a day later.
                self.touch_claim(&hash, &cache.node_id).await;
                return Ok(None);
            }
            Err(e) => {
                // A failure is different from an interruption: whatever is
                // staged was produced by something that then broke, and
                // resuming from it would build on that. Start clean next time.
                let _ = tokio::fs::remove_dir_all(&temp).await;
                let _ = self.store.forget_cache_entry(&hash, &cache.node_id).await;
                return Err(e);
            }
        };

        let final_dir = cache.dir.join(&relative);
        if let Some(parent) = final_dir.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::rename(&temp, &final_dir)
            .await
            .map_err(|e| format!("publishing {}: {e}", final_dir.display()))?;
        self.store
            .complete_cache_entry(&hash, &cache.node_id, published.bytes)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            recipe = %hash, file = file.id, height = target_height,
            bytes = published.bytes, duration_s = published.duration_ms / 1000,
            segments = published.segments, parts = published.parts,
            "pre-transcode published"
        );
        Ok(Some(Produced {
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
        file: &plurx_core::domain::MediaFile,
        opts: &TranscodeOptions,
        encoder: Encoder,
        hash: &str,
        deadline: Instant,
    ) -> Result<Option<Published>, String> {
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
            if Instant::now() >= deadline {
                tracing::debug!(recipe = %hash, "producer out of time for this run");
                return Ok(None);
            }
            // Do not even start while a viewer is queuing.
            //
            // For a hardware encoder the slot request answers this, since
            // background acquisition is refused outright while anyone is in the
            // queue. Software has no slot to ask for, and without the explicit
            // check this loop span: spawn ffmpeg, kill it on the first poll
            // microseconds later, spawn another — hundreds of processes a
            // second, for as long as somebody was waiting to play something.
            let slot = if encoder == Encoder::Software {
                if self.admissions.live_is_waiting() {
                    tokio::time::sleep(PRODUCER_RETRY).await;
                    continue;
                }
                None
            } else {
                match self.admissions.try_acquire(max, Priority::Background) {
                    Some(slot) => Some(slot),
                    None => {
                        tokio::time::sleep(PRODUCER_RETRY).await;
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
                ..opts.clone()
            };
            // Unpaced, deliberately. Pacing exists so a live session does not
            // write a film ahead of a playhead that will never reach it; a
            // producer has no playhead and every second it spends holding the
            // hardware is a second a viewer might want it.
            let args = transcode::hls_args(
                file,
                encoder,
                &part_opts,
                Pacing::unpaced(),
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
            )?;

            let ended = self.run_part(&mut child, deadline).await;
            drop(slot); // before anything else: a viewer is probably waiting on it
            let part = read_part(&part_dir).await;
            let produced = !part.is_empty();
            if produced {
                parts.push(part);
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
                    if matches!(ended, PartEnd::Deadline) {
                        // Out of budget for this pass. What was produced is
                        // discarded, because a partial asset must never be
                        // serveable and there is nowhere to record a
                        // checkpoint that outlives this call — resume is
                        // within a run, not across them. A film that cannot
                        // finish inside one six-hour window therefore never
                        // gets cached, which is a real limitation and a
                        // straightforward one to lift later: the parts are
                        // already named and numbered on disk, and the claim
                        // row is already the place a checkpoint would live.
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
    async fn run_part(&self, child: &mut Child, deadline: Instant) -> PartEnd {
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
            if self.admissions.live_is_waiting() {
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
    pub async fn create_session(
        &self,
        req: &SessionRequest,
        user_name: &str,
    ) -> Result<StartInfo, String> {
        let fingerprint = req.fingerprint();
        if let Some(key) = req.request_id.as_deref() {
            let known = self.requests.lock().await.get(key).cloned();
            if let Some((session_id, seen)) = known {
                if seen != fingerprint {
                    return Err(format!(
                        "request {key} was already used for a different stream"
                    ));
                }
                if let Some(info) = self.recover(&session_id).await {
                    tracing::debug!(%session_id, request_id = key, "idempotent create: same session");
                    return Ok(info);
                }
                // Its session is gone; the entry is stale, not authoritative.
                self.requests.lock().await.remove(key);
            }
        }

        let info = match req.kind {
            SessionKind::Transcode { height } => {
                self.start(
                    req.file_id,
                    height,
                    req.start_seconds,
                    req.audio_index,
                    req.subtitle_burn,
                    user_name,
                    &req.playback_id,
                )
                .await?
            }
            SessionKind::Copy { aac } => {
                self.start_copy(
                    req.file_id,
                    req.start_seconds,
                    req.audio_index,
                    aac,
                    user_name,
                    &req.playback_id,
                )
                .await?
            }
        };
        if let Some(key) = req.request_id.as_deref() {
            let mut requests = self.requests.lock().await;
            let live: std::collections::HashSet<String> =
                self.sessions.lock().await.keys().cloned().collect();
            // Drop entries whose session has ended, so this map cannot grow
            // for the life of the process.
            requests.retain(|_, (session_id, _)| live.contains(session_id));
            requests.insert(key.to_owned(), (info.session_id.clone(), fingerprint));
        }
        Ok(info)
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
        if self.encoder().await == Encoder::Software {
            return AUTO_SOFTWARE_HEIGHT;
        }
        source_height
            .filter(|h| *h > 0)
            .unwrap_or(AUTO_HARDWARE_MAX_HEIGHT)
            .clamp(MIN_HEIGHT, AUTO_HARDWARE_MAX_HEIGHT)
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
            // queue behind the session it just replaced.
            session.release_hardware();
            session.kill_child().await;
            session.discard_dir().await;
            tracing::info!(
                %session_id, playback_id,
                "reaped superseded transcode session (this player started a new one)"
            );
        }
    }

    /// Start a transcode session for a file, superseding this viewer's previous
    /// session on the same file (see [`Self::reap_superseded`]).
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
        // Before spawning, not after: the point is to never have two encoders
        // for one player running at once, and reaping first also frees the
        // hardware slot the new session is about to want.
        self.reap_superseded(playback_id).await;

        let file = self
            .store
            .get_file(file_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_owned())?;
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
        let opts = self.options_for(
            encoder,
            &file,
            target_height,
            start_seconds,
            audio_index,
            subtitle_burn.clone(),
        );
        if let Some(info) = self
            .serve_cached(&file, &opts, encoder, &item_title, user_name, playback_id)
            .await
        {
            return Ok(info);
        }

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
                    // whatever size you ask the output to be.
                    Admission::Software => {
                        tracing::info!(
                            file = file_id, class = %work.software_class(),
                            "hardware transcode slots full; this class runs comfortably                              in software here, so starting it there"
                        );
                        encoder = Encoder::Software;
                        break;
                    }
                    Admission::Refused(why) => {
                        tracing::warn!(file = file_id, class = %work.software_class(), "{why}");
                        return Err(why);
                    }
                }
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
        let opts = self.options_for(
            encoder,
            &file,
            target_height,
            start_seconds,
            audio_index,
            subtitle_burn,
        );
        let pacing = self.pacing(false).await;
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
            file.hdr.as_deref(),
            transcode::heavy_source(&file),
            opts.subtitle_burn.is_some(),
        );
        tracing::info!(
            %session_id, encoder = encoder.label(), pipeline = opts.pipeline.name(),
            proven = self.pipeline.name(), hdr = file.hdr.as_deref().unwrap_or("sdr"),
            declined = declined.unwrap_or(""),
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
        )?;

        tracing::info!(
            %session_id, file_id, target_height, start_seconds,
            encoder = encoder.label(), "started transcode session"
        );

        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(Some(child)),
            cached: false,
            last_access: Mutex::new(Instant::now()),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            start_seconds,
            target_height,
            encoder_label: Mutex::new(encoder.label()),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            progress: Arc::clone(&progress),
            class: work.class(if encoder == Encoder::Software {
                crate::admission::SOFTWARE
            } else {
                encoder.label()
            }),
            hw_slot: std::sync::Mutex::new(hw_slot),
            delivery: Meter::new(),
            suspended: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

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
            tokio::spawn(async move {
                if started_on_hardware {
                    tokio::time::sleep(FIRST_SEGMENT_GRACE).await;
                    if session_producing(&dir).await {
                        // Producing real segments. If the picture is still gray,
                        // the problem is the *output* (tone-map/color), not the
                        // pipeline stalling — this line says which. The speed
                        // says how much headroom it has while doing it.
                        tracing::info!(
                            session = %sid,
                            speed = session.progress.speed(),
                            "transcode producing segments within {}s (hardware path healthy)",
                            FIRST_SEGMENT_GRACE.as_secs()
                        );
                        return;
                    }
                    // Producing nothing, but *is* it stuck? A session stopped
                    // for running ahead obviously makes no progress, and one
                    // decoding 4K at 0.4x is slow rather than broken —
                    // restarting either on software trades a slow start for a
                    // slower one. Only a session whose output has actually
                    // stopped moving gets the fallback.
                    if session.suspended.load(Relaxed) {
                        return;
                    }
                    if session.progress.stalled_for() < PROGRESS_STALL {
                        tracing::info!(
                            session = %sid,
                            produced_ms = session.progress.out_time_ms(),
                            speed = session.progress.speed(),
                            "no finished segment yet, but the encoder is still advancing — \
                             letting it run rather than restarting it on something slower"
                        );
                        return;
                    }
                    // Downgrade by ONE step, and take the cheaper step first.
                    // A session running a GPU tone-map graph that stopped
                    // producing is evidence about the graph — a driver state
                    // the boot probe couldn't reach, a codec profile its
                    // fixture didn't cover, another session contending for the
                    // same block. None of that is evidence about the *encoder*,
                    // and swapping to software would trade a stalled hardware
                    // session for one that is slower still. So the graph goes
                    // and the hardware stays; only a session already on the CPU
                    // chain falls back to a software encoder.
                    //
                    // One step per session, deliberately: this watchdog fires
                    // once, and a viewer who has waited out the grace window
                    // twice has waited too long. If the downgraded session also
                    // stalls, the running-stall watchdog below takes it.
                    let downgrade_pipeline = opts.pipeline.on_gpu();
                    let retry_encoder = if downgrade_pipeline {
                        encoder
                    } else {
                        Encoder::Software
                    };
                    let mut retry_opts = opts.clone();
                    if downgrade_pipeline {
                        retry_opts.pipeline = opts.pipeline.fallback().unwrap_or(Pipeline::Cpu);
                    }
                    tracing::warn!(
                        session = %sid,
                        stalled_s = session.progress.stalled_for().as_secs(),
                        pipeline = opts.pipeline.name(),
                        retry_pipeline = retry_opts.pipeline.name(),
                        retry_encoder = retry_encoder.label(),
                        "no HLS segment from hardware within {}s and output has stopped \
                         advancing (GPU contention, or a decode the GPU can't do — e.g. \
                         Dolby Vision); {}",
                        FIRST_SEGMENT_GRACE.as_secs(),
                        if downgrade_pipeline {
                            "dropping the GPU tone-map and keeping the hardware encoder"
                        } else {
                            "retrying on software"
                        }
                    );
                    session.kill_child().await;
                    clear_session_dir(&dir).await;
                    let sw_args = transcode::hls_args(
                        &file,
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
                        &sid,
                        Arc::clone(&session.progress),
                        generation,
                    ) {
                        Ok(child) => {
                            *session.child.lock().await = Some(child);
                            *session.last_access.lock().await = Instant::now();
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
            encoder: encoder.label(),
            vod: false,
        })
    }

    /// Start a **copy-video** HLS session: the source video is repackaged into
    /// HLS (fMP4 segments) untouched, and only the audio is transcoded when the
    /// client can't take it. This is the remux path for players whose `<video>`
    /// won't accept a progressive fragmented MP4 (Safari) but decode HEVC/HDR
    /// natively via HLS — so the original 4K stream is preserved instead of the
    /// error-fallback re-encoding it down to 720p. No hardware/software encoder
    /// ladder (nothing is encoded), just a fail-fast guard.
    pub async fn start_copy(
        &self,
        file_id: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        transcode_audio: bool,
        user_name: &str,
        playback_id: &str,
    ) -> Result<StartInfo, String> {
        // Same reasoning as `start`; the copy path matters more if anything,
        // since an abandoned remux reads the source as fast as the disk allows.
        self.reap_superseded(playback_id).await;

        let file = self
            .store
            .get_file(file_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_owned())?;
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

        // Whether the DV strip may use dovi_rpu is an ffmpeg capability, and
        // the version line the cache config already carries is the record of
        // which ffmpeg this manager runs.
        let have_dovi = self
            .cache
            .as_ref()
            .map(|c| transcode::ffmpeg_has_dovi_bsf(&c.ffmpeg_build))
            .unwrap_or(false);
        let pacing = self.pacing(true).await;
        let legacy_args = || {
            transcode::hls_copy_args(
                &file,
                start_seconds,
                audio_override,
                transcode_audio,
                pacing,
                have_dovi,
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
            let args = transcode::copy_pipe_args(
                &file,
                start_seconds,
                audio_override,
                transcode_audio,
                pacing,
                have_dovi,
            );
            tracing::info!(
                %session_id, file_id, start_seconds, mode = "segmenter",
                "copy-video HLS ffmpeg args: {}", args.join(" ")
            );
            match spawn_ffmpeg_pipe(&args, &session_id, Arc::clone(&progress), generation) {
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
                    )?;
                    (child, None)
                }
            }
        } else {
            let args = legacy_args();
            tracing::info!(
                %session_id, file_id, start_seconds, mode = "legacy",
                "copy-video HLS ffmpeg args: {}", args.join(" ")
            );
            let child = spawn_ffmpeg(
                &args,
                "copy",
                &session_id,
                Arc::clone(&progress),
                generation,
            )?;
            (child, None)
        };

        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(Some(child)),
            cached: false,
            last_access: Mutex::new(Instant::now()),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            playback_id: playback_id.to_owned(),
            start_seconds,
            target_height: file.height.unwrap_or(0),
            encoder_label: Mutex::new("copy"),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            progress: Arc::clone(&progress),
            class: String::new(),
            hw_slot: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            suspended: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

        // The reader task, when plurx is doing the cutting. It owns the pipe
        // for the session's life, and it owns the one fallback: a stream it
        // cannot follow is killed and respawned on ffmpeg's HLS muxer, in this
        // same session, so the player never learns anything happened. Only
        // before the first segment — once a timeline exists on disk, a
        // respawn would rewrite one a player is already holding.
        if let Some(stdout) = pipe_stdout {
            let session = Arc::clone(&session);
            let sid = session_id.clone();
            let dir = dir.clone();
            let file = file.clone();
            let progress = Arc::clone(&progress);
            tokio::spawn(async move {
                let outcome =
                    copyseg::run(stdout, dir.clone(), &sid, copyseg::Limits::default()).await;
                match outcome {
                    copyseg::Outcome::Ran(counts) => {
                        tracing::info!(session = %sid, "{}", copyseg::summary(&counts));
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
                        let args = transcode::hls_copy_args(
                            &file,
                            start_seconds,
                            audio_override,
                            transcode_audio,
                            pacing,
                            have_dovi,
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
                        match spawn_ffmpeg(&args, "copy", &sid, progress, generation) {
                            Ok(child) => {
                                *session.child.lock().await = Some(child);
                                *session.last_access.lock().await = Instant::now();
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
            encoder: "copy",
            vod: false,
        })
    }

    /// Number of live transcode sessions (for /metrics).
    pub async fn active_sessions(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Everything the activity page shows about live sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().await;
        let mut out = Vec::with_capacity(sessions.len());
        for (id, s) in sessions.iter() {
            out.push(session_info(id, s).await);
        }
        out.sort_by_key(|s| s.started_unix);
        out
    }

    /// One session's live telemetry, for the player's stats overlay. Does not
    /// touch `last_access`: asking how a stream is doing is not the same as
    /// fetching from it, and a status poll must not keep an abandoned session
    /// alive past the idle reaper.
    pub async fn session_status(&self, session_id: &str) -> Option<SessionInfo> {
        let session = self.sessions.lock().await.get(session_id).cloned()?;
        Some(session_info(session_id, &session).await)
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
        session.kill_child().await;
        session.discard_dir().await;
        tracing::info!(%session_id, reason, "transcode session ended");
        true
    }

    async fn touch(&self, session_id: &str) -> Option<Arc<Session>> {
        let session = self.sessions.lock().await.get(session_id).cloned()?;
        *session.last_access.lock().await = Instant::now();
        Some(session)
    }

    /// Read the current media playlist for a session.
    pub async fn playlist(&self, session_id: &str) -> Option<Vec<u8>> {
        let session = self.touch(session_id).await?;
        let path = session.dir.join("index.m3u8");
        // The playlist appears a beat after ffmpeg starts; wait briefly. A failed
        // session returns None → 404 → the player reports an error rather than
        // polling a segment-less playlist on a gray screen forever.
        for _ in 0..100 {
            if session.failed.load(Relaxed) {
                return None;
            }
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if !bytes.is_empty() {
                    // The playlist just told us what is published — the one
                    // moment the segment index can be refreshed for free.
                    self.flow_control(&session, session_id).await;
                    return Some(bytes);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
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
        let session = self.touch(session_id).await?;
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
    /// Hysteresis is deliberate: resume at half the window rather than at the
    /// window, or a session sitting exactly on the boundary would toggle every
    /// tick. SIGKILL still works on a stopped process, so the idle reaper and
    /// the admin stop button need no special case; a suspended session that
    /// nobody comes back to is reaped on idle like any other.
    async fn apply_ahead_window(
        &self,
        session: &Session,
        session_id: &str,
        limits: AheadLimits,
        global_bytes: i64,
    ) {
        let Some(ahead) = session.ahead().await else {
            return; // nothing published yet — nothing to hold
        };
        let suspended = session.suspended.load(Relaxed);
        let want_suspend = should_suspend(ahead, global_bytes, limits, suspended);
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
        session.suspended.store(want_suspend, Relaxed);
        if want_suspend {
            tracing::debug!(
                session = %session_id,
                ahead_seconds = ahead.seconds, ahead_bytes = ahead.bytes,
                global_bytes, max_secs = limits.max_secs, max_bytes = limits.max_bytes,
                "suspending transcode: far enough ahead of the client"
            );
        } else {
            tracing::debug!(
                session = %session_id,
                ahead_seconds = ahead.seconds, ahead_bytes = ahead.bytes,
                "resuming transcode: the client caught up"
            );
        }
    }

    /// The bounds every session is held to, read once per evaluation.
    async fn ahead_limits(&self) -> AheadLimits {
        AheadLimits {
            max_secs: self
                .num_setting(keys::HLS_AHEAD_MAX_SECS, HLS_AHEAD_MAX_SECS_DEFAULT)
                .await,
            max_bytes: self
                .num_setting(keys::HLS_AHEAD_MAX_BYTES, HLS_AHEAD_MAX_BYTES_DEFAULT)
                .await,
            global_max_bytes: self
                .num_setting(keys::HLS_SCRATCH_MAX_BYTES, HLS_SCRATCH_MAX_BYTES_DEFAULT)
                .await,
        }
    }

    /// Scratch in use across every live session, from each one's cached
    /// figure — summing this must not cost a directory walk per session, or
    /// the flow controller could not run on every segment fetch.
    async fn global_ahead_bytes(&self) -> i64 {
        self.sessions
            .lock()
            .await
            .values()
            .map(|s| s.ahead_bytes.load(Relaxed))
            .sum()
    }

    /// Re-evaluate one session after something changed for it: a segment
    /// completed, or the client's frontier advanced.
    ///
    /// The reaper still sweeps every 15 seconds, but as a repair loop. A
    /// window that is only checked on a 15-second tick is not a flow
    /// controller — a fast encoder can put a great deal of 4K on disk between
    /// two ticks.
    async fn flow_control(&self, session: &Session, session_id: &str) {
        session.refresh_segments().await;
        let limits = self.ahead_limits().await;
        let global = self.global_ahead_bytes().await;
        self.apply_ahead_window(session, session_id, limits, global)
            .await;
    }

    /// Background loop: kill and remove sessions idle beyond the timeout,
    /// prune played-past segments, and hold sessions that have run ahead.
    pub async fn reap_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            let idle = Duration::from_secs(SESSION_IDLE_SECS);
            let limits = self.ahead_limits().await;
            let mut expired = Vec::new();
            let mut live = Vec::new();
            {
                let sessions = self.sessions.lock().await;
                for (id, s) in sessions.iter() {
                    if s.last_access.lock().await.elapsed() > idle {
                        expired.push((id.clone(), Arc::clone(s)));
                    } else {
                        live.push((id.clone(), Arc::clone(s)));
                    }
                }
            }
            for (id, session) in expired {
                self.sessions.lock().await.remove(&id);
                session.release_hardware();
                // Kills a suspended child too — SIGKILL is not blockable and
                // does not need the process scheduled to take effect.
                session.kill_child().await;
                session.discard_dir().await;
                tracing::info!(session_id = %id, "reaped idle transcode session");
            }
            // What this box actually achieves, remembered per class of work.
            // Admission asks it the next time hardware is full, so the answer
            // to "can software cope with this" is a measurement from this
            // machine rather than an assumption about machines in general.
            // A suspended session is making no progress on purpose and would
            // poison the record with a speed it was never asked to reach.
            for (_id, session) in &live {
                if session.class.is_empty() || session.suspended.load(Relaxed) {
                    continue;
                }
                if let Some(speed) = session.progress.recent_speed() {
                    self.admissions.record(&session.class, speed);
                }
            }
            // Repair pass. Flow control proper runs on segment completion and
            // frontier advance; this catches a session nobody is currently
            // fetching from, and prunes what has fallen out of retention.
            for (_id, session) in &live {
                session.refresh_segments().await;
                gc_expired_segments(session).await;
            }
            let global = self.global_ahead_bytes().await;
            for (id, session) in &live {
                self.apply_ahead_window(session, id, limits, global).await;
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
/// is not where the viewer is. With a 60-second client buffer, "15 segments
/// behind the frontier" could be *ahead* of the picture on screen.
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
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Software keeps the conservative answer whatever the source is: a
        // 1080p x264 session on a NUC cannot hold realtime, and a stream that
        // stutters at 1080p is worse than one that plays at 720p.
        for source in [Some(2160), Some(1080), Some(480), None] {
            assert_eq!(software.auto_height(source).await, 720, "source {source:?}");
        }

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
        let before = mgr
            .session_status(&info.session_id)
            .await
            .expect("status")
            .idle_seconds;
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
        assert!(mgr.touch(&info.session_id).await.is_some());
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
        // Running: hold once past any single window.
        assert!(!should_suspend(secs(179), 0, limits, false));
        assert!(should_suspend(secs(181), 0, limits, false));
        assert!(should_suspend(bytes(2_001), 0, limits, false));
        // The global budget holds a session that is individually well behaved
        // — several healthy 4K streams fill a disk between them.
        assert!(should_suspend(secs(10), 8_001, limits, false));

        // Held: keep holding until EVERY trigger is below half, so a session
        // parked on a boundary doesn't toggle on every evaluation.
        assert!(should_suspend(secs(120), 0, limits, true));
        assert!(!should_suspend(secs(90), 0, limits, true));
        assert!(should_suspend(secs(10), 4_001, limits, true));
        assert!(!should_suspend(secs(10), 4_000, limits, true));
        // Time is fine but bytes are not: still held.
        assert!(should_suspend(
            Ahead {
                seconds: 10,
                bytes: 1_001
            },
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
        assert!(!should_suspend(secs(10_000), 1 << 40, off, false));
        assert!(!should_suspend(secs(10_000), 1 << 40, off, true));
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
            last_access: Mutex::new(Instant::now()),
            file_id: 1,
            item_id: 1,
            item_title: "T".into(),
            user_name: "paul".into(),
            playback_id: "pb-test".into(),
            start_seconds: 0.0,
            target_height: 720,
            encoder_label: Mutex::new("test"),
            started_unix: 0,
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            fetched_end_ms: AtomicI64::new(0),
            segments: Mutex::new(SegmentIndex::default()),
            ahead_bytes: AtomicI64::new(0),
            progress: Arc::new(Progress::new()),
            class: String::new(),
            hw_slot: std::sync::Mutex::new(None),
            delivery: Meter::new(),
            suspended: AtomicBool::new(false),
        }
    }

    /// Build a session directory with a real playlist and real files, so the
    /// index, the retention window and the pruner are all exercised against
    /// what ffmpeg actually writes.
    async fn seeded_session_dir(dir: &std::path::Path, count: i64, secs_each: f64) {
        let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
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

    /// Retention is measured back from the DOWNLOAD frontier and must cover
    /// the client's forward buffer — the bug this replaced deleted media the
    /// viewer was about to watch, because it counted segments back from the
    /// furthest one *fetched* while the client had fetched a minute ahead of
    /// the picture on screen.
    #[tokio::test]
    async fn retention_covers_the_clients_forward_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        // 100 segments of 4s = 400s of media.
        seeded_session_dir(p, 100, 4.0).await;
        tokio::fs::write(p.join("init.mp4"), b"i")
            .await
            .expect("write init");

        let session = test_session(p.to_path_buf());
        session.refresh_segments().await;
        assert_eq!(
            session.segments.lock().await.produced_playable_end_ms(),
            Some(400_000)
        );

        // The client has fetched through 300s. Its playhead may be a full
        // forward buffer behind that.
        session.fetched_end_ms.store(300_000, Relaxed);
        gc_expired_segments(&session).await;

        // Everything within RETENTION_SECS of the frontier survives…
        let keep_from = 300_000 - RETENTION_SECS * 1000; // 180_000
        assert!(
            p.join(format!("seg{:05}.ts", keep_from / 4_000)).exists(),
            "media at the retention boundary is kept"
        );
        assert!(p.join("seg00074.ts").exists(), "just inside the window");
        // …and only what is older goes.
        assert!(!p.join("seg00000.ts").exists());
        assert!(!p.join("seg00010.ts").exists());
        assert!(p.join("init.mp4").exists(), "init is never a segment");

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
    async fn a_running_session_reports_progress_and_answers_to_signals() {
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

        let info = mgr
            .start_copy(file_id, 0.0, None, false, "paul", "pb-paul")
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
        let hold = AheadLimits {
            max_secs: 1,
            max_bytes: 1,
            global_max_bytes: 0,
        };
        let release = AheadLimits {
            max_secs: 0,
            max_bytes: 0,
            global_max_bytes: 0,
        };
        mgr.apply_ahead_window(&session, &info.session_id, hold, 0)
            .await;
        assert!(session.suspended.load(Relaxed), "session was held");
        let frozen = session.progress.out_time_ms();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            session.progress.out_time_ms(),
            frozen,
            "a suspended encoder produces nothing"
        );

        // Disabling the window resumes it — and it really resumes, rather than
        // just having a flag cleared.
        mgr.apply_ahead_window(&session, &info.session_id, release, 0)
            .await;
        assert!(!session.suspended.load(Relaxed), "session was released");
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
        mgr.apply_ahead_window(&session, &info.session_id, hold, 0)
            .await;
        assert!(session.suspended.load(Relaxed), "held again");
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
        let opts = mgr.options_for(encoder, file, height, 0.0, None, None);
        let mut digest = mgr.digest().expect("cache configured");
        digest.encoder = encoder;
        Recipe {
            digest: &digest,
            file,
            opts: &opts,
            audio_copied: false,
        }
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
        let opts = mgr.options_for(encoder, &file, 1080, 0.0, None, None);
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
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, 240);
        let file_id = seed_real_file(&store, &source).await;
        let (mgr, _work, cache) = cached_manager(&store);
        let file = store.get_file(file_id).await.expect("get").expect("file");
        let hash = recipe_hash_for(&mgr, &file, 240).await;

        // A short budget: it encodes some of the film and runs out of time.
        assert!(
            mgr.produce(&file, 240, Instant::now() + Duration::from_millis(700))
                .await
                .expect("produce")
                .is_none(),
            "an unfinished run must not publish"
        );

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
        let staging = crate::cachekeep::staging_dir(cache.path(), &hash);
        assert!(
            staging.join(crate::produce::part_dir(0)).exists(),
            "the encoded part was thrown away, so the next pass starts from zero"
        );

        // And picking it up finishes the job from where it stopped rather than
        // from the beginning.
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
            (made.duration_ms - 240_000).abs() <= 1_000,
            "resuming across passes lost picture: {}ms",
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

    /// Preempted mid-encode, then resumed — and the film that comes out has no
    /// hole in it.
    ///
    /// This is the producer's whole reason for existing in parts, and the
    /// failure it guards against is the quietest one in the system: resuming a
    /// few hundred milliseconds late loses a moment of picture in the middle of
    /// a film, in a file nobody watches until next week, with every log line
    /// green. Only measuring the assembled timeline against the source catches
    /// it, so that is what this does.
    #[tokio::test]
    async fn a_preempted_producer_resumes_without_losing_picture() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        // Long enough that the encode cannot finish between the interrupts.
        // Sized from measurement, not from taste: at 160x120 this fixture
        // transcodes in about three seconds here, and the 24-second version it
        // replaced took 394ms — which is to say the earlier version of this
        // test was interrupting an encode that had already finished, and
        // passing every assertion below without exercising a line of the
        // resume path. The `parts >= 2` check now makes that failure loud.
        const SECONDS: u32 = 240;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let source = media.path().join("Heat.mkv");
        write_real_video(&source, SECONDS);
        let file_id = seed_real_file(&store, &source).await;
        let (mgr, _work, cache) = cached_manager(&store);
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
        // Interrupt it twice, so the asset is assembled from at least three
        // parts and any per-join error would compound rather than cancel.
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let queued = mgr.admissions.wait_for_slot();
            // Held for longer than one poll interval, so the running encoder
            // cannot miss it.
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(queued);
        }
        let made = run
            .await
            .expect("join")
            .expect("produce")
            .expect("something was produced");
        // The assertion that stops this test passing for the wrong reason: a
        // fixture small enough to finish before the first interrupt would
        // satisfy every check below while exercising none of the resume path.
        assert!(
            made.parts >= 2,
            "the encode finished in one part — nothing was actually preempted, \
             so this test proved nothing about resuming"
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
        let opts = mgr.options_for(encoder, &file, 1080, 0.0, None, None);
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
        assert!(mgr.list_sessions().await.is_empty());

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
        let sessions = mgr.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_name, "paul");
        assert!(mgr.stop_session(&info.session_id, "test").await);
        assert_eq!(mgr.active_sessions().await, 0);

        // The copy-video path likewise creates and tears down a session.
        let info = mgr
            .start_copy(file_id, 5.0, Some(1), true, "paul", "pb-paul")
            .await
            .expect("start_copy");
        assert_eq!(info.encoder, "copy");
        assert!(mgr.stop_session(&info.session_id, "test").await);
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
            .start_copy(file_id, 0.0, None, false, "paul", "pb-paul")
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
        };

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
