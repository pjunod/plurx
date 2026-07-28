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
    self, Encoder, EncoderCaps, Pacing, ToneMap, TranscodeOptions, SEGMENT_SECONDS,
};
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::ffmpeg::{ffmpeg_bin, pacing_caps};

/// Idle timeout after which a session's ffmpeg is killed and its dir removed.
const SESSION_IDLE_SECS: u64 = 60;
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
/// How many segments behind the furthest-served one to keep on disk. An HLS
/// session's playlist grows for its whole life (event type), so without pruning
/// a full watch accumulates every segment — cheap at 720p, but a 4K copy-video
/// session at ~45 Mb/s would hoard ~17 GB. We delete segments well behind the
/// playhead; ~60 s (15 × 4 s) covers a player's back-buffer, and a seek restarts
/// the session anyway, so a played-past segment is never re-requested.
const KEEP_BEHIND_SEGMENTS: i64 = 15;
/// Default pace for an HLS session's input, as a multiple of realtime, and how
/// many seconds it may deliver flat-out first. Admin-overridable (see
/// [`keys::HLS_READRATE`] / [`keys::HLS_BURST_SECS`]).
pub(crate) const HLS_READRATE_DEFAULT: f64 = 2.0;
pub(crate) const HLS_BURST_SECS_DEFAULT: f64 = 90.0;
/// How far ahead of the playhead a session may write before it is suspended,
/// in seconds of content. See [`TranscodeManager::apply_ahead_window`].
pub(crate) const HLS_AHEAD_MAX_SECS_DEFAULT: i64 = 180;

/// Live encode telemetry for one session, fed by ffmpeg's `-progress` stream.
///
/// Without it, "slow" and "stalled" are the same observation. The only signal
/// the session machinery had was whether a finished segment was listed yet —
/// a yes/no answer to a question that needs a rate. A 4K HDR session
/// tone-mapping at 0.7x and a session whose GPU has wedged look identical for
/// the first twelve seconds, and the watchdog killed both, restarting the
/// merely-slow one on software that is slower still.
#[derive(Debug)]
struct Progress {
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
    fn new() -> Progress {
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
    fn begin_attempt(&self) -> u64 {
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

    fn out_time_ms(&self) -> Option<i64> {
        Some(self.out_time_ms.load(Relaxed)).filter(|v| *v >= 0)
    }

    fn speed(&self) -> Option<f64> {
        Some(self.speed_milli.load(Relaxed))
            .filter(|v| *v >= 0)
            .map(|v| v as f64 / 1000.0)
    }

    /// The rate over the last few seconds, which is the one that predicts a
    /// stall. Falls back to nothing rather than to the cumulative figure --
    /// reporting a session's lifetime average as "recent" is how a slowdown
    /// stays invisible.
    fn recent_speed(&self) -> Option<f64> {
        Some(self.recent_milli.load(Relaxed))
            .filter(|v| *v >= 0)
            .map(|v| v as f64 / 1000.0)
    }
}

/// Apply one `key=value` line of ffmpeg's `-progress` output.
///
/// Values can be the literal `N/A` (before the first frame lands, and for
/// `speed` on a copy that has not been running long enough to estimate),
/// which parses to nothing rather than to zero — zero would read as "stalled"
/// and zero would read as "not moving" respectively, both wrong.
fn apply_progress_line(progress: &Progress, generation: u64, line: &str) {
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
            matches!(child.try_wait(), Ok(Some(_)))
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
            {
                let mut child = session.child.lock().await;
                let _ = child.kill().await;
            }
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
    child: Mutex<Child>,
    last_access: Mutex<Instant>,
    // -- metadata for the activity page --
    file_id: i64,
    item_id: i64,
    item_title: String,
    user_name: String,
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
    /// Highest segment index the client has fetched (-1 before the first). The
    /// reaper prunes segments far enough behind this to bound disk use, and
    /// the ahead-window measures from it.
    high_segment: AtomicI64,
    /// Live encode telemetry (see [`Progress`]).
    progress: Arc<Progress>,
    /// True while the child is SIGSTOPped for running too far ahead of the
    /// playhead. Everything that judges a session's health has to know: a
    /// suspended encoder makes no progress *on purpose*.
    suspended: AtomicBool,
}

/// Seconds of content produced beyond what the client has fetched.
///
/// Both terms are session-relative — ffmpeg's `out_time` restarts at zero after
/// the input seek, and segment numbering restarts with the session — so the
/// subtraction needs no absolute timeline. Derived from telemetry rather than
/// by counting files on disk, which makes it exact to the frame, free to ask
/// for, and correct for the copy path (whose segment lengths follow the source
/// GOP and so can't be inferred from an index).
fn ahead_seconds(produced_ms: Option<i64>, high_segment: i64) -> Option<i64> {
    // `high_segment` is the last segment *fetched*, so the playhead sits at its
    // end; -1 (nothing fetched yet) puts the playhead at zero.
    let fetched_to = (high_segment + 1) * SEGMENT_SECONDS as i64;
    Some(produced_ms? / 1000 - fetched_to)
}

/// Whether a session should be held, given how far ahead it is and whether it
/// is already held.
///
/// The asymmetry is the point: suspend at the window, resume at half of it.
/// A single threshold makes a session sitting on the boundary toggle on every
/// reaper tick, which is a stream of signals and log lines to accomplish
/// nothing. Resuming early is the safe direction — the cost of being wrong is
/// some disk, and the cost of the other error is a viewer who runs dry.
fn should_suspend(ahead_seconds: i64, max_secs: i64, currently_suspended: bool) -> bool {
    if max_secs <= 0 {
        return false; // window disabled
    }
    if currently_suspended {
        ahead_seconds > max_secs / 2
    } else {
        ahead_seconds > max_secs
    }
}

impl Session {
    fn ahead_seconds(&self) -> Option<i64> {
        ahead_seconds(self.progress.out_time_ms(), self.high_segment.load(Relaxed))
    }
}

/// One live session as the activity page and the stats overlay see it.
async fn session_info(id: &str, s: &Session) -> SessionInfo {
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
        ahead_seconds: s.ahead_seconds(),
        suspended: s.suspended.load(Relaxed),
    }
}

pub struct StartInfo {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
    pub encoder: &'static str,
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
    /// Seconds of content written beyond the segment the client last fetched.
    pub ahead_seconds: Option<i64>,
    pub suspended: bool,
}

pub struct TranscodeManager {
    store: Arc<dyn Store>,
    work_dir: PathBuf,
    caps: EncoderCaps,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl TranscodeManager {
    pub fn new(store: Arc<dyn Store>, work_dir: PathBuf, caps: EncoderCaps) -> Self {
        TranscodeManager {
            store,
            work_dir,
            caps,
            sessions: Mutex::new(HashMap::new()),
        }
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

    /// Seconds of content a session may write beyond the client's playhead
    /// before it is suspended. `0` disables the window entirely.
    async fn ahead_max_secs(&self) -> i64 {
        self.num_setting(keys::HLS_AHEAD_MAX_SECS, HLS_AHEAD_MAX_SECS_DEFAULT)
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

    /// Kill any session this same viewer already has open on this same file,
    /// because the one they're about to start replaces it.
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
    /// Scoped to (viewer, file) rather than just viewer: one person legitimately
    /// watching two different things on two devices keeps both, while the seek
    /// case — same person, same file, moments apart — is always a replacement.
    /// Two devices on the same file under one account is the rare loser here,
    /// and it degrades to "the other device rebuffers and starts its own
    /// session", not to an error.
    async fn reap_superseded(&self, user_name: &str, file_id: i64) {
        let doomed: Vec<(String, Arc<Session>)> = {
            let mut sessions = self.sessions.lock().await;
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| s.file_id == file_id && s.user_name == user_name)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| sessions.remove(&id).map(|s| (id, s)))
                .collect()
        };
        for (session_id, session) in doomed {
            let _ = session.child.lock().await.kill().await;
            let _ = tokio::fs::remove_dir_all(&session.dir).await;
            tracing::info!(
                %session_id, file_id, user = user_name,
                "reaped superseded transcode session (client started a new one)"
            );
        }
    }

    /// Start a transcode session for a file, superseding this viewer's previous
    /// session on the same file (see [`Self::reap_superseded`]).
    pub async fn start(
        &self,
        file_id: i64,
        target_height: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        user_name: &str,
    ) -> Result<StartInfo, String> {
        // Before spawning, not after: the point is to never have two encoders
        // for one viewer running at once, and reaping first also frees the GPU
        // slot the new session is about to want.
        self.reap_superseded(user_name, file_id).await;

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

        let encoder = self.encoder().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("creating session dir: {e}"))?;

        // Default-track selection: prefer original (Japanese) audio + subs when
        // the file is dual-audio anime-style (REQ-SUB-2), and honor the
        // server-wide language preferences otherwise. Burn the chosen text
        // subtitle since HLS transcode delivers a single flat stream.
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
        let subtitle_burn = selection.subtitle_index.and_then(|idx| {
            let codec = file
                .subtitle_streams
                .get(idx as usize)
                .map(|s| s.codec.clone());
            // Only burn when we actively prefer original audio (dual-audio case).
            prefer_original.then_some(plurx_core::transcode::SubtitleBurn {
                subtitle_index: idx,
                bitmap: codec
                    .as_deref()
                    .map(plurx_core::tracks::is_bitmap_subtitle)
                    .unwrap_or(false),
            })
        });

        let opts = TranscodeOptions {
            target_height,
            video_bitrate_kbps: bitrate_for_height(target_height),
            // An explicit client choice (audio-language menu) wins over the
            // automatic dual-audio default.
            audio_index: audio_override.or(selection.audio_index),
            start_seconds,
            tone_map: tone_map_pref(),
            subtitle_burn,
            ..Default::default()
        };
        let pacing = self.pacing(false).await;
        let args = transcode::hls_args(&file, encoder, &opts, pacing, &dir.to_string_lossy());
        // Log the exact command — the single most useful diagnostic. It reveals
        // the decode/filter/encode pipeline actually used (e.g. whether heavy
        // HEVC is being hardware-decoded), and confirms which build is running.
        tracing::info!(
            %session_id, encoder = encoder.label(),
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
            child: Mutex::new(child),
            last_access: Mutex::new(Instant::now()),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            target_height,
            encoder_label: Mutex::new(encoder.label()),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            progress: Arc::clone(&progress),
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
                    tracing::warn!(
                        session = %sid,
                        stalled_s = session.progress.stalled_for().as_secs(),
                        "no HLS segment from hardware within {}s and output has stopped \
                         advancing (GPU contention, or a decode the GPU can't do — e.g. \
                         Dolby Vision); retrying on software",
                        FIRST_SEGMENT_GRACE.as_secs()
                    );
                    {
                        let mut child = session.child.lock().await;
                        let _ = child.kill().await;
                    }
                    clear_session_dir(&dir).await;
                    let sw_args = transcode::hls_args(
                        &file,
                        Encoder::Software,
                        &opts,
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
                        Encoder::Software.label(),
                        &sid,
                        Arc::clone(&session.progress),
                        generation,
                    ) {
                        Ok(child) => {
                            *session.child.lock().await = child;
                            *session.last_access.lock().await = Instant::now();
                            // The activity page must stop naming the hardware
                            // encoder the moment it is no longer the one running.
                            *session.encoder_label.lock().await = Encoder::Software.label();
                            tracing::info!(session = %sid, "software fallback transcode started");
                        }
                        Err(e) => {
                            tracing::error!(session = %sid, "software fallback failed: {e}");
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
    ) -> Result<StartInfo, String> {
        // Same reasoning as `start`; the copy path matters more if anything,
        // since an abandoned remux reads the source as fast as the disk allows.
        self.reap_superseded(user_name, file_id).await;

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

        let args = transcode::hls_copy_args(
            &file,
            start_seconds,
            audio_override,
            transcode_audio,
            self.pacing(true).await,
            &dir.to_string_lossy(),
        );
        tracing::info!(
            %session_id, file_id, start_seconds,
            "copy-video HLS ffmpeg args: {}", args.join(" ")
        );
        let progress = Arc::new(Progress::new());
        let generation = progress.begin_attempt();
        let child = spawn_ffmpeg(
            &args,
            "copy",
            &session_id,
            Arc::clone(&progress),
            generation,
        )?;

        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(child),
            last_access: Mutex::new(Instant::now()),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            target_height: file.height.unwrap_or(0),
            encoder_label: Mutex::new("copy"),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
            progress: Arc::clone(&progress),
            suspended: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

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

    /// Kill one session now (the activity page's stop button). True if it
    /// existed.
    pub async fn stop_session(&self, session_id: &str) -> bool {
        let Some(session) = self.sessions.lock().await.remove(session_id) else {
            return false;
        };
        let _ = session.child.lock().await.kill().await;
        let _ = tokio::fs::remove_dir_all(&session.dir).await;
        tracing::info!(%session_id, "transcode session stopped by admin");
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
                    return Some(bytes);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    /// Read a segment, waiting for ffmpeg to produce it if necessary.
    pub async fn segment(&self, session_id: &str, name: &str) -> Option<Vec<u8>> {
        // Guard against path traversal: segment names are `segNNNNN.ts` only.
        if !is_safe_segment(name) {
            return None;
        }
        let session = self.touch(session_id).await?;
        let path = session.dir.join(name);
        let idx = segment_index(name);

        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                // Track the playhead so the reaper can prune segments behind it.
                if let Some(i) = idx {
                    session.high_segment.fetch_max(i, Relaxed);
                }
                return Some(bytes);
            }
            // Give up if the session was declared dead, or ffmpeg has exited and
            // the file still isn't there.
            if session.failed.load(Relaxed) {
                return None;
            }
            let exited = {
                let mut child = session.child.lock().await;
                matches!(child.try_wait(), Ok(Some(_)))
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

    /// Hold a session that has run far enough ahead of the playhead, and let
    /// it go again once the viewer has caught up.
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
    async fn apply_ahead_window(&self, session: &Session, session_id: &str, max_secs: i64) {
        let Some(ahead) = session.ahead_seconds() else {
            return; // no telemetry yet — nothing produced, nothing to hold
        };
        let suspended = session.suspended.load(Relaxed);
        let want_suspend = should_suspend(ahead, max_secs, suspended);
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
            match child.id() {
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
                session = %session_id, ahead_seconds = ahead, max_secs,
                "suspending transcode: far enough ahead of the playhead"
            );
        } else {
            tracing::debug!(
                session = %session_id, ahead_seconds = ahead,
                "resuming transcode: the viewer caught up"
            );
        }
    }

    /// Background loop: kill and remove sessions idle beyond the timeout,
    /// prune played-past segments, and hold sessions that have run ahead.
    pub async fn reap_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            let idle = Duration::from_secs(SESSION_IDLE_SECS);
            let ahead_max = self.ahead_max_secs().await;
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
                // Kills a suspended child too — SIGKILL is not blockable and
                // does not need the process scheduled to take effect.
                let _ = session.child.lock().await.kill().await;
                let _ = tokio::fs::remove_dir_all(&session.dir).await;
                tracing::info!(session_id = %id, "reaped idle transcode session");
            }
            for (id, session) in live {
                // Bound disk on active sessions: an HLS playlist grows for the
                // whole session, so prune segments well behind the playhead (a
                // 4K copy session would otherwise hoard tens of GB)…
                gc_old_segments(&session.dir, session.high_segment.load(Relaxed)).await;
                // …and bound how far *ahead* of the playhead it may get.
                self.apply_ahead_window(&session, &id, ahead_max).await;
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

/// Delete segments far enough behind the furthest-served one to be safe. The
/// client restarts the session on any seek, so a played-past segment is never
/// re-requested; `init.mp4` and the playlist are always kept.
async fn gc_old_segments(dir: &std::path::Path, high: i64) {
    if high < KEEP_BEHIND_SEGMENTS {
        return; // not enough played yet to prune anything
    }
    let cutoff = high - KEEP_BEHIND_SEGMENTS;
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Some(i) = segment_index(&entry.file_name().to_string_lossy()) {
                if i < cutoff {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
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
        );
        let info = mgr
            .start(file_id, 720, 0.0, None, "paul")
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
        assert!(mgr.stop_session(&info.session_id).await);
    }

    #[test]
    fn ahead_of_the_playhead() {
        // 60s produced, nothing fetched → the whole 60s is runway.
        assert_eq!(ahead_seconds(Some(60_000), -1), Some(60));
        // Fetched segment 0 (0–4s) → 56s of runway at the default length.
        assert_eq!(
            ahead_seconds(Some(60_000), 0),
            Some(60 - SEGMENT_SECONDS as i64)
        );
        // A client that has caught up to the write head has no runway…
        assert_eq!(ahead_seconds(Some(40_000), 9), Some(0));
        // …and one fetching faster than production goes negative, which is
        // exactly the state a stall is made of.
        assert!(ahead_seconds(Some(40_000), 12).is_some_and(|a| a < 0));
        // No telemetry yet is not "zero ahead" — it is "don't know".
        assert_eq!(ahead_seconds(None, 5), None);
    }

    #[test]
    fn suspend_window_has_hysteresis() {
        // Running: hold only once past the window.
        assert!(!should_suspend(179, 180, false));
        assert!(should_suspend(181, 180, false));
        // Held: keep holding until the viewer has eaten half of it, so a
        // session parked on the boundary doesn't toggle every tick.
        assert!(should_suspend(120, 180, true));
        assert!(should_suspend(91, 180, true));
        assert!(!should_suspend(90, 180, true));
        // Disabled window never suspends, whatever the numbers say.
        assert!(!should_suspend(10_000, 0, false));
        assert!(!should_suspend(10_000, 0, true));
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
    async fn gc_prunes_segments_behind_playhead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        tokio::fs::write(p.join("init.mp4"), b"i")
            .await
            .expect("write init");
        for i in 0..=30 {
            tokio::fs::write(p.join(format!("seg{i:05}.m4s")), b"x")
                .await
                .expect("write seg");
        }
        // Playhead at 30 → cutoff = 30 - KEEP_BEHIND_SEGMENTS (15) = 15.
        gc_old_segments(p, 30).await;
        assert!(!p.join("seg00000.m4s").exists()); // behind → pruned
        assert!(!p.join("seg00014.m4s").exists()); // < cutoff → pruned
        assert!(p.join("seg00015.m4s").exists()); // == cutoff → kept
        assert!(p.join("seg00030.m4s").exists()); // playhead → kept
        assert!(p.join("init.mp4").exists()); // init always kept
    }

    #[tokio::test]
    async fn gc_keeps_everything_before_the_window_fills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        for i in 0..5 {
            tokio::fs::write(p.join(format!("seg{i:05}.ts")), b"x")
                .await
                .expect("write seg");
        }
        gc_old_segments(p, 4).await; // high < KEEP_BEHIND → nothing pruned
        assert!(p.join("seg00000.ts").exists());
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
            .start_copy(file_id, 0.0, None, false, "paul")
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

        // Nothing has been fetched, so everything produced is runway.
        assert!(
            session.ahead_seconds().is_some_and(|a| a > 0),
            "produced content with an unmoved playhead is ahead of the playhead"
        );

        // A window it has already exceeded suspends it, and a suspended
        // encoder stops advancing — which is the property the watchdog relies
        // on being able to tell apart from a wedge.
        mgr.apply_ahead_window(&session, &info.session_id, 1).await;
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
        mgr.apply_ahead_window(&session, &info.session_id, 0).await;
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
        mgr.apply_ahead_window(&session, &info.session_id, 1).await;
        assert!(session.suspended.load(Relaxed), "held again");
        assert!(mgr.stop_session(&info.session_id).await);
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
        assert!(!mgr.stop_session("missing").await);

        // A real start spawns ffmpeg (it fails async on the fake path, but the
        // session is created and tracked). Then the admin stop kills it.
        let info = mgr
            .start(file_id, 720, 0.0, None, "paul")
            .await
            .expect("start");
        assert_eq!(info.encoder, "software (x264)");
        assert_eq!(mgr.active_sessions().await, 1);
        let sessions = mgr.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].user_name, "paul");
        assert!(mgr.stop_session(&info.session_id).await);
        assert_eq!(mgr.active_sessions().await, 0);

        // The copy-video path likewise creates and tears down a session.
        let info = mgr
            .start_copy(file_id, 5.0, Some(1), true, "paul")
            .await
            .expect("start_copy");
        assert_eq!(info.encoder, "copy");
        assert!(mgr.stop_session(&info.session_id).await);
    }

    /// Seeking must not leave the old session running. Before this, every seek
    /// stacked another ffmpeg for up to ~75s (idle timeout + reaper tick).
    #[tokio::test]
    async fn a_new_session_supersedes_the_same_viewers_old_one() {
        super::require_ffmpeg();
        use plurx_core::store::SqliteStore;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let file_id = seed_file(&store).await;
        let work = tempfile::tempdir().expect("work");
        let mgr = TranscodeManager::new(
            Arc::clone(&store),
            work.path().to_path_buf(),
            EncoderCaps::default(),
        );

        // Play, then "seek" three times. Only the newest survives.
        let first = mgr
            .start(file_id, 720, 0.0, None, "paul")
            .await
            .expect("start");
        let second = mgr
            .start(file_id, 720, 600.0, None, "paul")
            .await
            .expect("seek");
        let third = mgr
            .start(file_id, 720, 1200.0, None, "paul")
            .await
            .expect("seek again");
        assert_eq!(mgr.active_sessions().await, 1);
        assert!(mgr.playlist(&first.session_id).await.is_none());
        assert!(!mgr.stop_session(&second.session_id).await);
        assert!(mgr.stop_session(&third.session_id).await);

        // The copy path supersedes too, and across paths: a transcode fallback
        // after a copy attempt must not leave the copy remux reading the disk.
        let copy = mgr
            .start_copy(file_id, 0.0, None, false, "paul")
            .await
            .expect("copy");
        let fallback = mgr
            .start(file_id, 720, 0.0, None, "paul")
            .await
            .expect("fallback");
        assert_eq!(mgr.active_sessions().await, 1);
        assert!(!mgr.stop_session(&copy.session_id).await);

        // Another viewer on the same file is untouched, and so is the same
        // viewer on a different file — only (viewer, file) is superseded.
        let other_viewer = mgr
            .start(file_id, 720, 0.0, None, "sam")
            .await
            .expect("other viewer");
        assert_eq!(mgr.active_sessions().await, 2);
        let reseek = mgr
            .start(file_id, 720, 30.0, None, "paul")
            .await
            .expect("paul seeks");
        assert_eq!(mgr.active_sessions().await, 2);
        assert!(!mgr.stop_session(&fallback.session_id).await);
        assert!(mgr.stop_session(&other_viewer.session_id).await);
        assert!(mgr.stop_session(&reseek.session_id).await);
        assert_eq!(mgr.active_sessions().await, 0);
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
