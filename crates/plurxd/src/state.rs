//! Shared application state and the background job manager.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use plurx_core::metadata::{self, EnrichReport, TmdbClient};
use plurx_core::scan::{self, ScanReport};
use plurx_core::store::{keys, Store};
use serde::Serialize;
use tokio::sync::Mutex;

/// Everything a request handler needs. Cheap to clone (all shared via `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub server_name: String,
    pub artwork_dir: PathBuf,
    pub jobs: Arc<JobManager>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(server_name: String, store: Arc<dyn Store>, artwork_dir: PathBuf) -> Self {
        let jobs = Arc::new(JobManager::new(Arc::clone(&store), artwork_dir.clone()));
        AppState {
            store,
            server_name,
            artwork_dir,
            jobs,
            started_at: Instant::now(),
        }
    }
}

/// Status of the most recent (or in-flight) scan for one library.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStatus {
    pub running: bool,
    pub last_scan: Option<ScanReport>,
    pub last_enrich: Option<EnrichReport>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
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
}

impl JobManager {
    fn new(store: Arc<dyn Store>, artwork_dir: PathBuf) -> Self {
        JobManager {
            store,
            artwork_dir,
            statuses: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot of all libraries' scan statuses.
    pub async fn all_statuses(&self) -> HashMap<i64, ScanStatus> {
        self.statuses.lock().await.clone()
    }

    /// Kick off a scan for `library_id` unless one is already running. Returns
    /// `true` if a scan was started, `false` if one was already in flight.
    pub async fn trigger_scan(self: &Arc<Self>, library_id: i64) -> bool {
        {
            let mut statuses = self.statuses.lock().await;
            let entry = statuses.entry(library_id).or_default();
            if entry.running {
                return false;
            }
            *entry = ScanStatus {
                running: true,
                started_at: Some(now()),
                ..Default::default()
            };
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_scan(library_id).await;
        });
        true
    }

    async fn run_scan(&self, library_id: i64) {
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

        match scan::scan_library(self.store.as_ref(), &library).await {
            Ok(report) => status.last_scan = Some(report),
            Err(e) => {
                self.finish(library_id, error_status(&e.to_string())).await;
                return;
            }
        }

        // Enrich only if a TMDB key is configured; absence is not an error.
        match self.store.get_setting(keys::TMDB_API_KEY).await {
            Ok(Some(key)) if !key.is_empty() => {
                let tmdb = TmdbClient::new(key);
                let report = metadata::enrich_library(
                    self.store.as_ref(),
                    &tmdb,
                    &self.artwork_dir,
                    Some(library_id),
                )
                .await;
                status.last_enrich = Some(report);
            }
            Ok(_) => tracing::info!("no TMDB key configured; skipping enrichment"),
            Err(e) => tracing::warn!(error = %e, "reading TMDB key"),
        }

        status.running = false;
        status.finished_at = Some(now());
        self.finish(library_id, status).await;
    }

    async fn finish(&self, library_id: i64, mut status: ScanStatus) {
        status.running = false;
        if status.finished_at.is_none() {
            status.finished_at = Some(now());
        }
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
