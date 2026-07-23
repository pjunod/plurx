//! On-the-fly HLS transcode sessions.
//!
//! When the decision engine says a file must be transcoded (HEVC/4K/HDR the
//! device can't take), we spawn one ffmpeg per session producing HLS segments
//! into a temp dir, and serve the playlist and segments over HTTP. Sessions
//! are reaped when idle. This is the session-based model; the deterministic
//! per-segment model that enables cluster failover is Phase 3's spike.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plurx_core::store::{keys, Store};
use plurx_core::transcode::{self, Encoder, EncoderCaps, ToneMap, TranscodeOptions};
use tokio::process::Child;
use tokio::sync::Mutex;

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
/// How many segments behind the furthest-served one to keep on disk. An HLS
/// session's playlist grows for its whole life (event type), so without pruning
/// a full watch accumulates every segment — cheap at 720p, but a 4K copy-video
/// session at ~45 Mb/s would hoard ~17 GB. We delete segments well behind the
/// playhead; ~60 s (15 × 4 s) covers a player's back-buffer, and a seek restarts
/// the session anyway, so a played-past segment is never re-requested.
const KEEP_BEHIND_SEGMENTS: i64 = 15;

/// Spawn an ffmpeg HLS transcode, draining its stderr (at `-loglevel error`)
/// into the logs so a failure is visible instead of a silently dead session.
fn spawn_ffmpeg(
    args: &[String],
    encoder_label: &'static str,
    session_id: &str,
) -> Result<Child, String> {
    let mut child = tokio::process::Command::new(ffmpeg_bin())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ffmpeg: {e}"))?;
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

/// Remove the (empty/partial) HLS output so a restarted ffmpeg starts clean.
async fn clear_session_dir(dir: &std::path::Path) {
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

fn ffmpeg_bin() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
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
    encoder_label: &'static str,
    started_unix: i64,
    /// Set when the session can never produce output (hardware and software
    /// both failed to emit a first segment). Playlist/segment reads then fail
    /// fast so the player shows an error instead of waiting on a gray screen.
    failed: AtomicBool,
    /// Highest segment index the client has fetched (-1 before the first). The
    /// reaper prunes segments far enough behind this to bound disk use.
    high_segment: AtomicI64,
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

    /// Start (or return a matching) transcode session for a file.
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

    pub async fn start(
        &self,
        file_id: i64,
        target_height: i64,
        start_seconds: f64,
        audio_override: Option<i64>,
        user_name: &str,
    ) -> Result<StartInfo, String> {
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
        let args = transcode::hls_args(&file, encoder, &opts, &dir.to_string_lossy());
        // Log the exact command — the single most useful diagnostic. It reveals
        // the decode/filter/encode pipeline actually used (e.g. whether heavy
        // HEVC is being hardware-decoded), and confirms which build is running.
        tracing::info!(
            %session_id, encoder = encoder.label(),
            "transcode ffmpeg args: {}", args.join(" ")
        );
        let child = spawn_ffmpeg(&args, encoder.label(), &session_id)?;

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
            encoder_label: encoder.label(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
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
                        // pipeline stalling — this line says which.
                        tracing::info!(
                            session = %sid,
                            "transcode producing segments within {}s (hardware path healthy)",
                            FIRST_SEGMENT_GRACE.as_secs()
                        );
                        return;
                    }
                    tracing::warn!(
                        session = %sid,
                        "no HLS segment from hardware within {}s (GPU contention, or a decode \
                         the GPU can't do — e.g. Dolby Vision); retrying on software",
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
                        &dir.to_string_lossy(),
                    );
                    match spawn_ffmpeg(&sw_args, Encoder::Software.label(), &sid) {
                        Ok(child) => {
                            *session.child.lock().await = child;
                            *session.last_access.lock().await = Instant::now();
                            tracing::info!(session = %sid, "software fallback transcode started");
                        }
                        Err(e) => {
                            tracing::error!(session = %sid, "software fallback failed: {e}");
                            session.failed.store(true, Relaxed);
                            return;
                        }
                    }
                }

                // Fail-fast guard: whatever is running now (software, or a
                // software-from-the-start session) must produce a first segment
                // in the software window, or the session is declared dead.
                tokio::time::sleep(SOFTWARE_GRACE).await;
                if !session_producing(&dir).await {
                    tracing::error!(
                        session = %sid,
                        "no HLS segment within {}s of software transcode; failing the session — \
                         the source is likely undecodable by this ffmpeg build (e.g. a Dolby \
                         Vision profile it can't handle)",
                        SOFTWARE_GRACE.as_secs()
                    );
                    {
                        let mut child = session.child.lock().await;
                        let _ = child.kill().await;
                    }
                    session.failed.store(true, Relaxed);
                }
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
            &dir.to_string_lossy(),
        );
        tracing::info!(
            %session_id, file_id, start_seconds,
            "copy-video HLS ffmpeg args: {}", args.join(" ")
        );
        let child = spawn_ffmpeg(&args, "copy", &session_id)?;

        let session = Arc::new(Session {
            dir: dir.clone(),
            child: Mutex::new(child),
            last_access: Mutex::new(Instant::now()),
            file_id,
            item_id: file.item_id,
            item_title,
            user_name: user_name.to_owned(),
            target_height: file.height.unwrap_or(0),
            encoder_label: "copy",
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            failed: AtomicBool::new(false),
            high_segment: AtomicI64::new(-1),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

        // Fail-fast guard: copy has no encoder ladder, but if the first segment
        // never lands (undecodable source, ffmpeg refusal) mark the session
        // failed so the player errors instead of waiting on a gray screen.
        {
            let session = Arc::clone(&session);
            let dir = dir.clone();
            let sid = session_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SOFTWARE_GRACE).await;
                if !session_producing(&dir).await {
                    tracing::error!(
                        session = %sid,
                        "no HLS segment within {}s of copy-video session; failing it",
                        SOFTWARE_GRACE.as_secs()
                    );
                    {
                        let mut child = session.child.lock().await;
                        let _ = child.kill().await;
                    }
                    session.failed.store(true, Relaxed);
                }
            });
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
            out.push(SessionInfo {
                id: id.clone(),
                file_id: s.file_id,
                item_id: s.item_id,
                item_title: s.item_title.clone(),
                user_name: s.user_name.clone(),
                target_height: s.target_height,
                encoder: s.encoder_label,
                started_unix: s.started_unix,
                idle_seconds: s.last_access.lock().await.elapsed().as_secs(),
            });
        }
        out.sort_by_key(|s| s.started_unix);
        out
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

    /// Background loop: kill and remove sessions idle beyond the timeout.
    pub async fn reap_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            let idle = Duration::from_secs(SESSION_IDLE_SECS);
            let mut expired = Vec::new();
            let mut live = Vec::new(); // (dir, high_segment) for behind-playhead GC
            {
                let sessions = self.sessions.lock().await;
                for (id, s) in sessions.iter() {
                    if s.last_access.lock().await.elapsed() > idle {
                        expired.push((id.clone(), Arc::clone(s)));
                    } else {
                        live.push((s.dir.clone(), s.high_segment.load(Relaxed)));
                    }
                }
            }
            for (id, session) in expired {
                self.sessions.lock().await.remove(&id);
                let _ = session.child.lock().await.kill().await;
                let _ = tokio::fs::remove_dir_all(&session.dir).await;
                tracing::info!(session_id = %id, "reaped idle transcode session");
            }
            // Bound disk on active sessions: an HLS playlist grows for the whole
            // session, so prune segments well behind the playhead (a 4K copy
            // session would otherwise hoard tens of GB).
            for (dir, high) in live {
                gc_old_segments(&dir, high).await;
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
