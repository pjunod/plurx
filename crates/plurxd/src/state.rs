//! Shared application state and the background job manager.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use plurx_core::metadata::{self, AniListClient, EnrichReport, TmdbClient};
use plurx_core::scan::{self, ScanProgress, ScanReport};
use plurx_core::store::{keys, Store};
use plurx_core::transcode::EncoderCaps;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::logbuf::LogBuffer;
use crate::trakt::TraktManager;
use crate::transcode::TranscodeManager;

/// Environment facts collected once at startup, shown on the settings page.
/// Everything here is admin-facing diagnostics — paths, tool versions,
/// detected hardware — not runtime state.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemInfo {
    pub data_dir: String,
    pub ffmpeg: String,
    pub ffprobe: String,
    /// First line of `ffmpeg -version`, if ffmpeg ran at all.
    pub ffmpeg_version: Option<String>,
    /// PLURX_HWACCEL preference, or "auto".
    pub hwaccel_pref: String,
    pub encoders: EncoderCaps,
    /// Human label of the encoder the transcoder will actually pick.
    pub encoder_selected: String,
}

/// Everything a request handler needs. Cheap to clone (all shared via `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub server_name: String,
    pub artwork_dir: PathBuf,
    pub jobs: Arc<JobManager>,
    pub transcode: Arc<TranscodeManager>,
    pub trakt: Arc<TraktManager>,
    pub system: Arc<SystemInfo>,
    pub logs: Arc<LogBuffer>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(
        server_name: String,
        store: Arc<dyn Store>,
        artwork_dir: PathBuf,
        transcode_dir: PathBuf,
        encoder_caps: EncoderCaps,
        system: SystemInfo,
        logs: Arc<LogBuffer>,
    ) -> Self {
        let jobs = Arc::new(JobManager::new(Arc::clone(&store), artwork_dir.clone()));
        let transcode = Arc::new(TranscodeManager::new(
            Arc::clone(&store),
            transcode_dir,
            encoder_caps,
        ));
        // PLURX_TRAKT_BASE overrides the API base for tests/mocks.
        let trakt_base = std::env::var("PLURX_TRAKT_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| plurx_core::trakt::DEFAULT_BASE.to_owned());
        let trakt = Arc::new(TraktManager::new(Arc::clone(&store), trakt_base));
        AppState {
            store,
            server_name,
            artwork_dir,
            jobs,
            transcode,
            trakt,
            system: Arc::new(system),
            logs,
            started_at: Instant::now(),
        }
    }
}

/// Status of the most recent (or in-flight) scan for one library.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStatus {
    pub running: bool,
    /// What the job is doing right now: "scanning" or "enriching".
    pub phase: Option<String>,
    /// Live counters while running (sampled from the scan's atomics).
    pub progress: Option<ProgressSnapshot>,
    pub last_scan: Option<ScanReport>,
    pub last_enrich: Option<EnrichReport>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// Point-in-time view of a running scan's counters.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ProgressSnapshot {
    pub found: usize,
    pub processed: usize,
    pub changed: usize,
}

impl ProgressSnapshot {
    fn sample(p: &ScanProgress) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        ProgressSnapshot {
            found: p.found.load(Relaxed),
            processed: p.processed.load(Relaxed),
            changed: p.changed.load(Relaxed),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Runs library scans (and metadata enrichment) off the request path, one at a
/// time per library. In Phase 4 this becomes a leader-scheduled cluster
/// singleton (ARCHITECTURE §2.2); the API surface here stays the same.
pub struct JobManager {
    store: Arc<dyn Store>,
    artwork_dir: PathBuf,
    statuses: Mutex<HashMap<i64, ScanStatus>>,
    /// Live counters for in-flight scans, sampled by `all_statuses`.
    live: Mutex<HashMap<i64, Arc<ScanProgress>>>,
}

impl JobManager {
    fn new(store: Arc<dyn Store>, artwork_dir: PathBuf) -> Self {
        JobManager {
            store,
            artwork_dir,
            statuses: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot of all libraries' scan statuses, with live progress attached
    /// to any scan currently running.
    pub async fn all_statuses(&self) -> HashMap<i64, ScanStatus> {
        let mut map = self.statuses.lock().await.clone();
        let live = self.live.lock().await;
        for (id, progress) in live.iter() {
            if let Some(status) = map.get_mut(id) {
                if status.running {
                    status.progress = Some(ProgressSnapshot::sample(progress));
                }
            }
        }
        map
    }

    /// Kick off a scan for `library_id` unless one is already running. Returns
    /// `true` if a scan was started, `false` if one was already in flight.
    pub async fn trigger_scan(self: &Arc<Self>, library_id: i64) -> bool {
        self.trigger(library_id, false).await
    }

    /// Like [`trigger_scan`], but forces a full metadata refresh — re-enriches
    /// even already-matched items (backfills season posters onto older shows).
    pub async fn trigger_refresh(self: &Arc<Self>, library_id: i64) -> bool {
        self.trigger(library_id, true).await
    }

    async fn trigger(self: &Arc<Self>, library_id: i64, force_metadata: bool) -> bool {
        {
            let mut statuses = self.statuses.lock().await;
            let entry = statuses.entry(library_id).or_default();
            if entry.running {
                return false;
            }
            *entry = ScanStatus {
                running: true,
                phase: Some("scanning".to_owned()),
                started_at: Some(now()),
                ..Default::default()
            };
        }
        let progress = Arc::new(ScanProgress::default());
        self.live
            .lock()
            .await
            .insert(library_id, Arc::clone(&progress));

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_scan(library_id, progress, force_metadata).await;
        });
        true
    }

    async fn run_scan(&self, library_id: i64, progress: Arc<ScanProgress>, force_metadata: bool) {
        let mut status = ScanStatus {
            running: true,
            started_at: Some(now()),
            ..Default::default()
        };

        let library = match self.store.get_library(library_id).await {
            Ok(Some(lib)) => lib,
            Ok(None) => {
                self.finish(library_id, error_status("library not found"))
                    .await;
                return;
            }
            Err(e) => {
                self.finish(library_id, error_status(&e.to_string())).await;
                return;
            }
        };

        match scan::scan_library_with_progress(self.store.as_ref(), &library, Some(&progress)).await
        {
            Ok(report) => status.last_scan = Some(report),
            Err(e) => {
                self.finish(library_id, error_status(&e.to_string())).await;
                return;
            }
        }

        // Publish the scan result before enrichment starts, so the UI shows
        // real counts (and any problems) while metadata is still fetching.
        {
            let mut statuses = self.statuses.lock().await;
            if let Some(entry) = statuses.get_mut(&library_id) {
                entry.last_scan = status.last_scan.clone();
                entry.phase = Some("enriching".to_owned());
            }
        }

        // Anime libraries enrich from AniList (no key needed); everything else
        // from TMDB when a key is configured.
        if library.anime {
            let client = AniListClient::new();
            let report = metadata::enrich_anime_library(
                self.store.as_ref(),
                &client,
                &self.artwork_dir,
                library_id,
                force_metadata,
            )
            .await;
            status.last_enrich = Some(report);
        } else {
            match self.store.get_setting(keys::TMDB_API_KEY).await {
                Ok(Some(key)) if !key.is_empty() => {
                    let tmdb = TmdbClient::new(key);
                    let report = metadata::enrich_library(
                        self.store.as_ref(),
                        &tmdb,
                        &self.artwork_dir,
                        Some(library_id),
                        force_metadata,
                    )
                    .await;
                    status.last_enrich = Some(report);
                }
                Ok(_) => tracing::info!("no TMDB key configured; skipping enrichment"),
                Err(e) => tracing::warn!(error = %e, "reading TMDB key"),
            }
        }

        status.running = false;
        status.finished_at = Some(now());
        self.finish(library_id, status).await;
    }

    async fn finish(&self, library_id: i64, mut status: ScanStatus) {
        status.running = false;
        status.phase = None;
        status.progress = None;
        if status.finished_at.is_none() {
            status.finished_at = Some(now());
        }
        self.live.lock().await.remove(&library_id);
        self.statuses.lock().await.insert(library_id, status);
    }
}

fn error_status(message: &str) -> ScanStatus {
    ScanStatus {
        running: false,
        finished_at: Some(now()),
        error: Some(message.to_owned()),
        ..Default::default()
    }
}
