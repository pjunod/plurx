//! Shared application state and the background job manager.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use plurx_core::domain::{LibraryKind, MetadataPatch};
use plurx_core::metadata::local::LocalArtReport;
use plurx_core::metadata::{self, AniListClient, EnrichReport, TmdbClient};
use plurx_core::scan::{self, PlacedFile, ScanProgress, ScanReport, TargetError, TargetedScan};
use plurx_core::store::{keys, Store};
use plurx_core::transcode::EncoderCaps;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::logbuf::LogBuffer;
use crate::schedule::{due_jobs, DueJob, GlobalSchedule};
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
    /// What the tone-map probe found at boot: the graph this node uses, and
    /// what each rejected candidate failed on.
    ///
    /// Here rather than in a field of its own because it is the same kind of
    /// fact as the encoder list — something measured about this machine at
    /// startup and true for the process's life. Worth surfacing rather than
    /// leaving in the log, because falling back to the CPU chain is *silent*:
    /// everything plays, 4K just stays slow. "This box has no GPU tone-map"
    /// and "the driver refused the graph" are the difference between shrugging
    /// and installing a package.
    pub tone_map: crate::pipeprobe::PipelineReport,
}

/// The daemon's directories, all under the configured data dir.
///
/// Grouped rather than passed loose because the distinction between them
/// matters and is easy to get backwards positionally: `transcode` is scratch
/// and is wiped at every boot, while `cache` holds finished pre-transcodes
/// and must survive restarts — swapping the two would erase the cache on
/// startup and leave stale segments behind forever.
#[derive(Clone, Debug, Default)]
pub struct Dirs {
    pub artwork: PathBuf,
    pub transcode: PathBuf,
    pub cache: PathBuf,
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
    /// The coming-soon rail's cached answer from monarr (plan §11.2).
    pub coming_soon: Arc<crate::http::ComingSoonCache>,
    /// Pushes watch state to monarr when enabled (plan §11.1).
    pub watched: Arc<crate::watched::WatchedNotifier>,
    /// Keeps the click path off the NAS, and announces a start once playback
    /// is real rather than once a decision has been made.
    pub availability: Arc<crate::playstart::AvailabilityCache>,
    pub starts: Arc<crate::playstart::StartNotifier>,
    /// Live telemetry for progressive `/stream.mp4` remuxes, which are not
    /// transcode sessions and so have nowhere else to report from.
    pub streams: Arc<crate::progressive::Streams>,
    pub started_at: Instant,
}

impl AppState {
    /// `node_id` is this server's stable id — the `node_id` a cache location
    /// is recorded against, so a cluster can tell whose copy is whose.
    pub fn new(
        server_name: String,
        store: Arc<dyn Store>,
        dirs: Dirs,
        node_id: String,
        encoder_caps: EncoderCaps,
        system: SystemInfo,
        logs: Arc<LogBuffer>,
    ) -> Self {
        let Dirs {
            artwork: artwork_dir,
            transcode: transcode_dir,
            cache: cache_dir,
        } = dirs;
        let jobs = Arc::new(JobManager::new(Arc::clone(&store), artwork_dir.clone()));
        let coming_soon = crate::http::ComingSoonCache::new();
        let watched = crate::watched::WatchedNotifier::new(Arc::clone(&store));
        let transcode = Arc::new(
            TranscodeManager::new(
                Arc::clone(&store),
                transcode_dir,
                encoder_caps,
                system.tone_map.selected(),
            )
            .with_cache(
                cache_dir,
                system.ffmpeg_version.clone().unwrap_or_default(),
                node_id,
            ),
        );
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
            coming_soon,
            watched,
            availability: Arc::new(crate::playstart::AvailabilityCache::new()),
            starts: Arc::new(crate::playstart::StartNotifier::new()),
            streams: crate::progressive::Streams::new(),
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
    /// Home libraries only: the local-artwork pass (frame grabs, adopted
    /// sidecar images, inherited folder posters) that stands in for a provider.
    pub last_local_art: Option<LocalArtReport>,
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
/// What asked for a scan. It is a label on a counter, but the distinction is
/// the point of having the counter: "plurx scanned 400 times today" says
/// nothing, while "398 of them were scheduled and 2 were targeted" says the
/// fast path is not being used and something upstream is not calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanTrigger {
    /// A person pressed a button, or created or edited a library.
    Manual,
    /// The reconcile interval came round (P5).
    Scheduled,
    /// Scan-at-startup, covering what landed while the server was off.
    Startup,
    /// Another application said "index exactly this path".
    Targeted,
}

impl ScanTrigger {
    pub fn label(self) -> &'static str {
        match self {
            ScanTrigger::Manual => "manual",
            ScanTrigger::Scheduled => "scheduled",
            ScanTrigger::Startup => "startup",
            ScanTrigger::Targeted => "targeted",
        }
    }
}

/// Counters for the integration, exposed on `/metrics`.
///
/// Deliberately counters and not gauges: the question these answer is "is
/// the other application actually talking to us, and has it stopped?", and
/// only a monotonic count can distinguish "never" from "not since the last
/// restart" once it is graphed.
#[derive(Default, Debug)]
pub struct IntegrationMetrics {
    manual: AtomicU64,
    scheduled: AtomicU64,
    startup: AtomicU64,
    targeted: AtomicU64,
    notify_received: AtomicU64,
}

impl IntegrationMetrics {
    fn count_scan(&self, trigger: ScanTrigger) {
        self.count_for(trigger).fetch_add(1, Ordering::Relaxed);
    }

    /// Every inbound scan request that reached the handler, counted before
    /// the path is resolved — a request rejected for a path-mapping mistake
    /// still proves the other application reached plurx, which is the first
    /// thing anyone debugging this needs to know.
    pub fn count_notification(&self) {
        self.notify_received.fetch_add(1, Ordering::Relaxed);
    }

    /// `(trigger label, count)` pairs, plus notifications received.
    ///
    /// Every trigger is listed even at zero. A counter that only appears
    /// once it fires cannot express "this has never happened", which is the
    /// single most useful thing `plurx_scan_total{trigger="targeted"}` has
    /// to say.
    pub fn snapshot(&self) -> (Vec<(&'static str, u64)>, u64) {
        let counts = [
            ScanTrigger::Manual,
            ScanTrigger::Scheduled,
            ScanTrigger::Startup,
            ScanTrigger::Targeted,
        ]
        .into_iter()
        .map(|t| (t.label(), self.count_for(t).load(Ordering::Relaxed)))
        .collect();
        (counts, self.notify_received.load(Ordering::Relaxed))
    }

    fn count_for(&self, trigger: ScanTrigger) -> &AtomicU64 {
        match trigger {
            ScanTrigger::Manual => &self.manual,
            ScanTrigger::Scheduled => &self.scheduled,
            ScanTrigger::Startup => &self.startup,
            ScanTrigger::Targeted => &self.targeted,
        }
    }
}

pub struct JobManager {
    store: Arc<dyn Store>,
    artwork_dir: PathBuf,
    statuses: Mutex<HashMap<i64, ScanStatus>>,
    /// Live counters for in-flight scans, sampled by `all_statuses`.
    live: Mutex<HashMap<i64, Arc<ScanProgress>>>,
    /// Targeted scans waiting for a library's running scan to finish,
    /// per library. See [`JobManager::request_scan`].
    pending: Mutex<HashMap<i64, Vec<ScanRequest>>>,
    /// Recent targeted-scan requests and their outcomes, newest last.
    requests: Mutex<VecDeque<ScanRequestRecord>>,
    metrics: IntegrationMetrics,
}

/// Ids the caller already knows, so plurx does not have to guess.
///
/// Without these, matching is title+year parsed off a filename — the step
/// that puts the wrong poster on a remake. The caller grabbed a specific
/// TMDB id; telling plurx costs nothing and ends the ambiguity.
#[derive(Clone, Debug, Default)]
pub struct IdHints {
    pub tmdb: Option<i64>,
    pub imdb: Option<String>,
    /// For an episode, the SHOW's id — an episode's own id is not what
    /// identifies the series it belongs to.
    pub series_tmdb: Option<i64>,
    /// The ids belong to an ancestor (a show), not to the placed item.
    pub episodeish: bool,
}

impl IdHints {
    fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.imdb.is_none() && self.series_tmdb.is_none()
    }
}

/// One "scan exactly this" ask.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub id: String,
    pub library_id: i64,
    pub path: PathBuf,
    /// Applied by the JOB, not by the endpoint — a request served from the
    /// pending queue must apply its ids too, and an endpoint that applied
    /// them itself would silently drop them for every request that arrived
    /// while a scan was running. Which is most of them.
    pub ids: Option<IdHints>,
    pub correlation_id: Option<String>,
    pub source: Option<String>,
}

/// What happened to a request, for the operator and for the caller polling
/// its status.
#[derive(Clone, Debug, Serialize)]
pub struct ScanRequestRecord {
    pub request_id: String,
    pub at: i64,
    pub library_id: i64,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// queued | running | done | failed
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ScanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<PlacedFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How many request records are kept. A debugging surface — "what happened
/// last night" — not an audit log, and deliberately in memory: persisting it
/// would mean a schema, a retention policy and a growth problem, for data
/// whose value expires in hours.
const MAX_REQUESTS: usize = 256;

impl JobManager {
    fn new(store: Arc<dyn Store>, artwork_dir: PathBuf) -> Self {
        JobManager {
            store,
            artwork_dir,
            statuses: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(VecDeque::new()),
            metrics: IntegrationMetrics::default(),
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
        self.trigger_scan_as(library_id, ScanTrigger::Manual).await
    }

    /// [`trigger_scan`], saying what asked for it.
    pub async fn trigger_scan_as(self: &Arc<Self>, library_id: i64, why: ScanTrigger) -> bool {
        self.trigger(library_id, false, why).await
    }

    /// Counters for `/metrics` and the system page.
    pub fn metrics(&self) -> &IntegrationMetrics {
        &self.metrics
    }

    /// Like [`trigger_scan`], but forces a full metadata refresh — re-enriches
    /// even already-matched items (backfills season posters onto older shows).
    pub async fn trigger_refresh(self: &Arc<Self>, library_id: i64) -> bool {
        self.trigger(library_id, true, ScanTrigger::Manual).await
    }

    /// [`trigger_refresh`], saying what asked for it.
    pub async fn trigger_refresh_as(self: &Arc<Self>, library_id: i64, why: ScanTrigger) -> bool {
        self.trigger(library_id, true, why).await
    }

    async fn trigger(
        self: &Arc<Self>,
        library_id: i64,
        force_metadata: bool,
        why: ScanTrigger,
    ) -> bool {
        {
            let mut statuses = self.statuses.lock().await;
            let entry = statuses.entry(library_id).or_default();
            if entry.running {
                return false;
            }
            self.metrics.count_scan(why);
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
            // Whatever queued up while this ran is work someone was
            // promised. A full scan covers the same files a targeted one
            // would have, but the CALLER is still owed its answer — the
            // item ids it asked for — so the queue drains rather than
            // being discarded as redundant.
            manager.drain_pending(library_id).await;
        });
        true
    }

    /// Ask for a targeted scan of one path.
    ///
    /// Returns `Ok(Some(scan))` when it ran now, or `Ok(None)` when the
    /// library was already scanning and the request was queued — the caller
    /// polls `scan_request` for the outcome.
    ///
    /// **Requests are coalesced, never dropped.** `trigger` returns false
    /// while a scan runs, which is right for "the user pressed Scan twice"
    /// and wrong here: importing a season fires one request per episode
    /// within seconds, and dropping N−1 of them would leave most of the
    /// season unindexed with nothing anywhere saying so. Duplicates by path
    /// collapse (scanning the same folder twice is the same work), and the
    /// rest are drained when the running scan finishes.
    pub async fn request_scan(
        self: &Arc<Self>,
        req: ScanRequest,
    ) -> Result<Option<TargetedScan>, TargetError> {
        self.record_request(&req, "running", None, None).await;

        let busy = {
            let statuses = self.statuses.lock().await;
            statuses.get(&req.library_id).is_some_and(|s| s.running)
        };
        if busy {
            let mut pending = self.pending.lock().await;
            let queue = pending.entry(req.library_id).or_default();
            if queue.iter().any(|q| q.path == req.path) {
                // The same folder is already waiting. Scanning it twice
                // would produce the same rows and the same answer.
                tracing::debug!(target: "plurxd::integrate",
                    path = %req.path.display(), "targeted scan already pending; coalesced");
            } else {
                queue.push(req.clone());
            }
            drop(pending);
            self.set_request_status(&req.id, "queued").await;
            return Ok(None);
        }

        let out = self.run_targeted(&req).await;
        match &out {
            Ok(scan) => {
                self.record_request(&req, "done", Some(scan), None).await;
            }
            Err(e) => {
                self.record_request(&req, "failed", None, Some(e.to_string()))
                    .await;
            }
        }
        out.map(Some)
    }

    async fn run_targeted(&self, req: &ScanRequest) -> Result<TargetedScan, TargetError> {
        self.metrics.count_scan(ScanTrigger::Targeted);
        let library = self
            .store
            .get_library(req.library_id)
            .await
            .map_err(TargetError::Store)?
            .ok_or_else(|| TargetError::OutsideRoots {
                path: req.path.display().to_string(),
                roots: Vec::new(),
            })?;
        tracing::info!(
            target: "plurxd::integrate",
            library = req.library_id,
            path = %req.path.display(),
            correlation_id = req.correlation_id.as_deref().unwrap_or("-"),
            source = req.source.as_deref().unwrap_or("-"),
            request = %req.id,
            "targeted scan requested"
        );
        let out = scan::scan_path(self.store.as_ref(), &library, &req.path).await?;
        self.apply_ids(req, &out.items).await;
        Ok(out)
    }

    /// Apply caller-supplied ids to what the scan placed.
    ///
    /// Best-effort by design: a failure here must not fail the scan. The
    /// files are indexed and playable either way, and a missing id degrades
    /// to the fuzzy title match plurx would have done anyway.
    async fn apply_ids(&self, req: &ScanRequest, items: &[PlacedFile]) {
        let Some(ids) = req.ids.as_ref().filter(|i| !i.is_empty()) else {
            return;
        };
        for placed in items {
            let target = if ids.episodeish {
                match self.show_root(placed.item_id).await {
                    Some(id) => id,
                    None => continue,
                }
            } else {
                placed.item_id
            };
            let patch = MetadataPatch {
                tmdb_id: ids.series_tmdb.or(ids.tmdb),
                imdb_id: if ids.episodeish {
                    None
                } else {
                    ids.imdb.clone()
                },
                ..Default::default()
            };
            match self.store.apply_metadata(target, &patch).await {
                Ok(_) => tracing::info!(
                    target: "plurxd::integrate",
                    item = target,
                    tmdb = patch.tmdb_id.unwrap_or(0),
                    correlation_id = req.correlation_id.as_deref().unwrap_or("-"),
                    "applied caller-supplied ids"
                ),
                Err(e) => tracing::warn!(
                    target: "plurxd::integrate",
                    item = target, error = %e,
                    correlation_id = req.correlation_id.as_deref().unwrap_or("-"),
                    "could not apply caller-supplied ids; falling back to title matching"
                ),
            }
        }
    }

    /// Walk up to the item that carries a show's identity: an episode's ids
    /// belong to its series, not to the episode row.
    async fn show_root(&self, item_id: i64) -> Option<i64> {
        let mut current = self.store.get_item(item_id).await.ok()??;
        for _ in 0..4 {
            match current.parent_id {
                Some(parent) => current = self.store.get_item(parent).await.ok()??,
                None => break,
            }
        }
        Some(current.id)
    }

    /// Run whatever queued up while a library was scanning. Called once a
    /// scan finishes; the work a caller was promised must not be forgotten
    /// just because it arrived at a busy moment.
    async fn drain_pending(self: &Arc<Self>, library_id: i64) {
        let queued = {
            let mut pending = self.pending.lock().await;
            pending.remove(&library_id).unwrap_or_default()
        };
        for req in queued {
            let out = self.run_targeted(&req).await;
            match &out {
                Ok(scan) => self.record_request(&req, "done", Some(scan), None).await,
                Err(e) => {
                    self.record_request(&req, "failed", None, Some(e.to_string()))
                        .await
                }
            }
        }
    }

    /// The recent request ring, newest last.
    pub async fn scan_requests(&self) -> Vec<ScanRequestRecord> {
        self.requests.lock().await.iter().cloned().collect()
    }

    pub async fn scan_request(&self, id: &str) -> Option<ScanRequestRecord> {
        self.requests
            .lock()
            .await
            .iter()
            .find(|r| r.request_id == id)
            .cloned()
    }

    async fn record_request(
        &self,
        req: &ScanRequest,
        status: &str,
        scan: Option<&TargetedScan>,
        error: Option<String>,
    ) {
        let mut ring = self.requests.lock().await;
        if let Some(existing) = ring.iter_mut().find(|r| r.request_id == req.id) {
            existing.status = status.to_owned();
            existing.report = scan.map(|s| s.report.clone());
            existing.items = scan.map(|s| s.items.clone());
            existing.error = error;
            return;
        }
        if ring.len() == MAX_REQUESTS {
            ring.pop_front();
        }
        ring.push_back(ScanRequestRecord {
            request_id: req.id.clone(),
            at: now(),
            library_id: req.library_id,
            path: req.path.display().to_string(),
            correlation_id: req.correlation_id.clone(),
            source: req.source.clone(),
            status: status.to_owned(),
            report: scan.map(|s| s.report.clone()),
            items: scan.map(|s| s.items.clone()),
            error,
        });
    }

    async fn set_request_status(&self, id: &str, status: &str) {
        if let Some(r) = self
            .requests
            .lock()
            .await
            .iter_mut()
            .find(|r| r.request_id == id)
        {
            r.status = status.to_owned();
        }
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

        // Home libraries have no provider at all: their enrichment is local
        // artwork (frame grabs and adopted sidecar images). Anime libraries
        // enrich from AniList (no key needed); everything else from TMDB when
        // a key is configured.
        if library.kind == LibraryKind::Home {
            let report = metadata::local::enrich_home_library(
                self.store.as_ref(),
                &self.artwork_dir,
                library_id,
                force_metadata,
            )
            .await;
            status.last_local_art = Some(report);
        } else if library.anime {
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
        // Stamp the schedule from the *end* of the run, not the start: a scan
        // that takes 40 minutes on a 1-hour interval would otherwise be due
        // again 20 minutes later, and a library slower than its own interval
        // would scan without pause.
        if let Err(e) = self
            .store
            .mark_library_scanned(library_id, force_metadata)
            .await
        {
            tracing::warn!(error = %e, library = library_id, "recording the run time failed");
        }
        self.finish(library_id, status).await;
    }

    /// The scheduler: ask [`crate::schedule::due_jobs`] once a minute, dispatch
    /// what it says, stamp what it ran.
    ///
    /// A minute is the resolution because the intervals are minutes and nothing
    /// here is urgent; the cost is one small query per tick. Scheduled and
    /// manual runs go through the same `trigger_*` methods, so a scheduled scan
    /// can't stack on top of a running one — `trigger` refuses, and the next
    /// tick tries again.
    pub async fn schedule_loop(self: Arc<Self>, transcode: Arc<TranscodeManager>) {
        self.scan_on_startup().await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(e) = self.run_due_jobs(&transcode).await {
                tracing::warn!(error = %e, "scheduler tick failed");
            }
        }
    }

    /// Scan every library once at boot, if the operator asked for it.
    ///
    /// The interval schedule alone can't cover a server that was *off* while
    /// files landed: its clock only starts when the process does, so a machine
    /// powered up at noon on a daily schedule ignores everything added
    /// overnight until midnight. Off by default, like every other job.
    ///
    /// The delay is not politeness — a scan competes with the first plays of
    /// the morning for the same disks — and it is also what keeps a crash-loop
    /// from turning into a scan-loop against the media volume.
    async fn scan_on_startup(self: &Arc<Self>) {
        const SETTLE: std::time::Duration = std::time::Duration::from_secs(30);
        match self.store.get_setting(keys::JOB_SCAN_ON_STARTUP).await {
            Ok(Some(v)) if v.trim() == "1" => {}
            _ => return,
        }
        tokio::time::sleep(SETTLE).await;
        let libraries = match self.store.list_libraries().await {
            Ok(libraries) => libraries,
            Err(e) => {
                tracing::warn!(error = %e, "startup scan skipped: cannot list libraries");
                return;
            }
        };
        for library in libraries {
            if self.trigger_scan_as(library.id, ScanTrigger::Startup).await {
                tracing::info!(library = library.id, name = %library.name, "startup scan started");
            }
        }
    }

    async fn run_due_jobs(
        self: &Arc<Self>,
        transcode: &Arc<TranscodeManager>,
    ) -> Result<(), plurx_core::error::StoreError> {
        let libraries = self.store.list_libraries().await?;
        let global = GlobalSchedule {
            probe_retry_mins: self.job_interval(keys::JOB_PROBE_RETRY_MINS).await,
            last_probe_retry: self.job_stamp(keys::JOB_LAST_PROBE_RETRY).await,
            transcode_cleanup_mins: self.job_interval(keys::JOB_TRANSCODE_CLEANUP_MINS).await,
            last_transcode_cleanup: self.job_stamp(keys::JOB_LAST_TRANSCODE_CLEANUP).await,
        };
        for job in due_jobs(now(), &libraries, global) {
            match job {
                DueJob::Scan(id) => {
                    if self.trigger_scan_as(id, ScanTrigger::Scheduled).await {
                        tracing::info!(library = id, "scheduled scan started");
                    }
                }
                DueJob::Refresh(id) => {
                    if self.trigger_refresh_as(id, ScanTrigger::Scheduled).await {
                        tracing::info!(library = id, "scheduled metadata refresh started");
                    }
                }
                // The server-wide jobs are stamped *before* they run and run
                // inline on this task: both are short, and stamping first means
                // one that fails can't retry at a job per minute forever.
                DueJob::RetryProbes => {
                    self.stamp(keys::JOB_LAST_PROBE_RETRY).await;
                    let files = self.store.files_missing_probe(None).await?;
                    if !files.is_empty() {
                        let report = scan::reprobe_files(self.store.as_ref(), &files).await?;
                        tracing::info!(
                            attempted = report.attempted,
                            repaired = report.repaired,
                            still_failing = report.still_failing,
                            "scheduled re-probe finished"
                        );
                    }
                }
                DueJob::CleanupTranscode => {
                    self.stamp(keys::JOB_LAST_TRANSCODE_CLEANUP).await;
                    let removed = transcode.sweep_orphan_dirs().await;
                    if removed > 0 {
                        tracing::info!(removed, "swept orphaned transcode directories");
                    }
                }
            }
        }
        Ok(())
    }

    /// A minutes-interval setting; absent, blank or unparseable reads as off.
    /// Deliberately not an error: a hand-edited settings row should stop one
    /// job, not the server.
    async fn job_interval(&self, key: &str) -> i64 {
        self.store
            .get_setting(key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    async fn job_stamp(&self, key: &str) -> Option<i64> {
        self.store
            .get_setting(key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<i64>().ok())
    }

    async fn stamp(&self, key: &str) {
        if let Err(e) = self.store.put_setting(key, &now().to_string()).await {
            tracing::warn!(error = %e, key, "recording a job run time failed");
        }
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
