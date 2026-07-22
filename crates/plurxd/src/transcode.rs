//! On-the-fly HLS transcode sessions.
//!
//! When the decision engine says a file must be transcoded (HEVC/4K/HDR the
//! device can't take), we spawn one ffmpeg per session producing HLS segments
//! into a temp dir, and serve the playlist and segments over HTTP. Sessions
//! are reaped when idle. This is the session-based model; the deterministic
//! per-segment model that enables cluster failover is Phase 3's spike.

use std::collections::HashMap;
use std::path::PathBuf;
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

fn ffmpeg_bin() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

fn tone_map_pref() -> ToneMap {
    match std::env::var("PLURX_TONEMAP").as_deref() {
        Ok("libplacebo") => ToneMap::Libplacebo,
        _ => ToneMap::Zscale,
    }
}

struct Session {
    dir: PathBuf,
    child: Mutex<Child>,
    last_access: Mutex<Instant>,
}

pub struct StartInfo {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
    pub encoder: &'static str,
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
    pub async fn start(
        &self,
        file_id: i64,
        target_height: i64,
        start_seconds: f64,
    ) -> Result<StartInfo, String> {
        let file = self
            .store
            .get_file(file_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_owned())?;

        let encoder = self.encoder().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("creating session dir: {e}"))?;

        // Default-track selection: prefer original (Japanese) audio + subs when
        // the file is dual-audio anime-style (REQ-SUB-2). Burn the chosen text
        // subtitle since HLS transcode delivers a single flat stream.
        let prefer_original = file
            .audio_streams
            .iter()
            .any(|a| matches!(a.language.as_deref(), Some("jpn" | "ja" | "jp")))
            && file.audio_streams.len() > 1;
        let selection = plurx_core::tracks::select_tracks(
            &file.audio_streams,
            &file.subtitle_streams,
            prefer_original,
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
            audio_index: selection.audio_index,
            start_seconds,
            tone_map: tone_map_pref(),
            subtitle_burn,
            ..Default::default()
        };
        let args = transcode::hls_args(&file, encoder, &opts, &dir.to_string_lossy());

        let mut child = tokio::process::Command::new(ffmpeg_bin())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawning ffmpeg: {e}"))?;

        // Drain ffmpeg's stderr into the logs. It runs at -loglevel error, so
        // anything here is a real failure — e.g. a hardware encoder that
        // validated but chokes on the actual filter graph. Without this the
        // session just dies and the client shows a blank player.
        if let Some(stderr) = child.stderr.take() {
            let sid = session_id.clone();
            let enc = encoder.label();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(session = %sid, encoder = enc, "transcode ffmpeg: {line}");
                }
            });
        }

        tracing::info!(
            %session_id, file_id, target_height, start_seconds,
            encoder = encoder.label(), "started transcode session"
        );

        let session = Arc::new(Session {
            dir,
            child: Mutex::new(child),
            last_access: Mutex::new(Instant::now()),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

        Ok(StartInfo {
            playlist_url: format!("/api/v1/hls/{session_id}/index.m3u8"),
            session_id,
            duration_ms: file.duration_ms,
            start_seconds,
            encoder: encoder.label(),
        })
    }

    /// Number of live transcode sessions (for /metrics).
    pub async fn active_sessions(&self) -> usize {
        self.sessions.lock().await.len()
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
        // The playlist appears a beat after ffmpeg starts; wait briefly.
        for _ in 0..100 {
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

        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                return Some(bytes);
            }
            // If ffmpeg has exited and the file still isn't there, give up.
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
            {
                let sessions = self.sessions.lock().await;
                for (id, s) in sessions.iter() {
                    if s.last_access.lock().await.elapsed() > idle {
                        expired.push((id.clone(), Arc::clone(s)));
                    }
                }
            }
            for (id, session) in expired {
                self.sessions.lock().await.remove(&id);
                let _ = session.child.lock().await.kill().await;
                let _ = tokio::fs::remove_dir_all(&session.dir).await;
                tracing::info!(session_id = %id, "reaped idle transcode session");
            }
        }
    }
}

/// Only `segNNNNN.ts` names are valid segment requests.
fn is_safe_segment(name: &str) -> bool {
    name.strip_prefix("seg")
        .and_then(|rest| rest.strip_suffix(".ts"))
        .map(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
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
        assert!(!is_safe_segment("seg.ts"));
        assert!(!is_safe_segment("../seg00000.ts"));
        assert!(!is_safe_segment("index.m3u8"));
        assert!(!is_safe_segment("seg0/../../etc.ts"));
    }

    #[test]
    fn bitrate_ladder() {
        assert_eq!(bitrate_for_height(2160), 20_000);
        assert_eq!(bitrate_for_height(1080), 8_000);
        assert_eq!(bitrate_for_height(720), 4_000);
        assert_eq!(bitrate_for_height(240), 1_200);
    }
}
