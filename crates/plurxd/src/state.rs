//! Shared application state and the background job manager.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use plurx_core::domain::{Library, LibraryKind, MetadataPatch};
use plurx_core::error::StoreError;
use plurx_core::metadata::genres::GenreBackfillReport;
use plurx_core::metadata::local::LocalArtReport;
use plurx_core::metadata::{self, AniListClient, EnrichReport, TmdbClient};
use plurx_core::scan::{self, PlacedFile, ScanProgress, ScanReport, TargetError, TargetedScan};
use plurx_core::store::{keys, Store};
use plurx_core::transcode::EncoderCaps;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::logbuf::LogBuffer;
use crate::offline::OfflineManager;
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
    /// Whether this ffmpeg can strip a Dolby Vision configuration
    /// (`dovi_rpu`, 7.1+) — probed at boot, not inferred from the version
    /// line. It decides a *verdict*, not just a filter argument: without it a
    /// DV source has to be re-encoded for any browser that cannot decode DV,
    /// instead of remuxed to its HDR10 base. Surfaced because the symptom
    /// (a 4K film quietly playing at the Auto rung in Chrome and perfectly in
    /// Safari) is otherwise unattributable from outside the machine.
    pub dovi_rpu: bool,
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
    /// Extracted-subtitle cache (review §3.3). A sibling of the transcode
    /// cache under `cache/`, for the same reason that one is: it survives
    /// restarts, and the transcode scratch — which is cleared at boot and
    /// swept for orphans — must not contain it.
    pub subs: PathBuf,
}

/// Everything a request handler needs. Cheap to clone (all shared via `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub server_name: String,
    /// Stable identity of the node that owns local transcode/offline bytes.
    pub node_id: String,
    pub artwork_dir: PathBuf,
    /// Finished content-addressed transcodes. Offline routes never join a
    /// request-controlled path directly to this root.
    pub cache_dir: PathBuf,
    /// Where extracted subtitles are kept, keyed by file identity and source
    /// fingerprint — see `http::stream::subtitles_vtt`.
    pub subs_dir: PathBuf,
    /// Staged rollout gate for the authenticated `pgs-v1` overlay API. The
    /// daemon only advertises the capability when the same process will serve
    /// it. Default-off until physical-client acceptance is complete.
    pub pgs_overlay_enabled: bool,
    pub jobs: Arc<JobManager>,
    pub transcode: Arc<TranscodeManager>,
    pub offline: Arc<OfflineManager>,
    pub trakt: Arc<TraktManager>,
    pub system: Arc<SystemInfo>,
    pub logs: Arc<LogBuffer>,
    /// The coming-soon rail's cached answer from monarr (plan §11.2).
    pub coming_soon: Arc<crate::http::ComingSoonCache>,
    /// Pushes watch state to monarr when enabled (plan §11.1).
    pub watched: Arc<crate::watched::WatchedNotifier>,
    /// What the storage under each library actually reads at
    /// (`crate::storeprobe`). Behind a lock and not in `SystemInfo` because,
    /// unlike the encoder list, it is not a fact about the machine that is
    /// true for the process's life: a mount can be re-exported, a link can be
    /// re-cabled, and the number is re-measurable on demand.
    ///
    /// Filled by a background task shortly after boot rather than during it —
    /// the numbers are worth having, but not at the price of a server that
    /// will not answer until a sleeping array has spun up.
    pub storage: Arc<tokio::sync::RwLock<crate::storeprobe::StorageReport>>,
    /// Keeps the click path off the NAS, and announces a start once playback
    /// is real rather than once a decision has been made.
    pub availability: Arc<crate::playstart::AvailabilityCache>,
    pub starts: Arc<crate::playstart::StartNotifier>,
    /// Live telemetry for progressive `/stream.mp4` remuxes, which are not
    /// transcode sessions and so have nowhere else to report from.
    pub streams: Arc<crate::progressive::Streams>,
    /// Deliveries that keep no record of themselves — direct play. The other
    /// three routes are listed from the machinery that already owns their
    /// lifetimes, so this holds only what would otherwise be invisible; see
    /// [`crate::delivery`].
    pub direct_plays: Arc<crate::delivery::DirectPlays>,
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
            subs: subs_dir,
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
            .with_dv_strippable(system.dovi_rpu)
            .with_cache(
                cache_dir.clone(),
                system.ffmpeg_version.clone().unwrap_or_default(),
                node_id.clone(),
            ),
        );
        // PLURX_TRAKT_BASE overrides the API base for tests/mocks.
        let trakt_base = std::env::var("PLURX_TRAKT_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| plurx_core::trakt::DEFAULT_BASE.to_owned());
        let trakt = Arc::new(TraktManager::new(Arc::clone(&store), trakt_base));
        let offline =
            OfflineManager::new(Arc::clone(&store), Arc::clone(&transcode), node_id.clone());
        AppState {
            store,
            server_name,
            node_id,
            artwork_dir,
            cache_dir,
            subs_dir,
            pgs_overlay_enabled: std::env::var("PLURX_PGS_OVERLAY").is_ok_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            jobs,
            transcode,
            offline,
            trakt,
            system: Arc::new(system),
            logs,
            coming_soon,
            watched,
            storage: Arc::new(tokio::sync::RwLock::new(Default::default())),
            availability: Arc::new(crate::playstart::AvailabilityCache::new()),
            starts: Arc::new(crate::playstart::StartNotifier::new()),
            streams: crate::progressive::Streams::new(),
            direct_plays: crate::delivery::DirectPlays::new(),
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

/// What one enrichment pass produced. Exactly the two status fields a scan
/// records, so the single enrichment path can hand them back to a caller that
/// keeps a `ScanStatus` (the full scan) and to one that does not (a targeted
/// scan, the retry sweep, a per-item refresh) without either learning which
/// kind of provider ran.
#[derive(Clone, Debug, Default, Serialize)]
pub struct EnrichOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrich: Option<EnrichReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_art: Option<LocalArtReport>,
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
    /// Test-only provider override so the targeted-scan seam can be exercised
    /// through the real job manager without reaching the public TMDB API.
    #[cfg(test)]
    tmdb_base: Option<(String, String)>,
    statuses: Mutex<HashMap<i64, ScanStatus>>,
    /// Live counters for in-flight scans, sampled by `all_statuses`.
    live: Mutex<HashMap<i64, Arc<ScanProgress>>>,
    /// Targeted scans waiting for a library's running scan to finish,
    /// per library. See [`JobManager::request_scan`].
    pending: Mutex<HashMap<i64, Vec<ScanRequest>>>,
    /// Recent targeted-scan requests and their outcomes, newest last.
    requests: Mutex<VecDeque<ScanRequestRecord>>,
    metrics: IntegrationMetrics,
    /// A pre-transcode pass is running. Not a mutex, because the answer wanted
    /// is "is one going" rather than "wait for it": a second pass would fight
    /// the first for the same slots, and queuing one behind an encode that
    /// takes hours is worse than skipping it.
    producing: std::sync::atomic::AtomicBool,
    /// Which title the pass is on, for the activity feed.
    ///
    /// The flag above answers "may another pass start"; this answers "what is
    /// using the GPU, and why", which is the question an admin looking at a
    /// busy ffmpeg actually has. Without it the producer holds an encoder for
    /// up to six hours with no entry in the activity feed, no session in the
    /// UI and nothing to stop — `ps auxwww` on the box was the only way to
    /// find out, which is not an acceptable answer for someone's own server.
    now_producing: Mutex<Option<ProducingNow>>,
    /// Set to ask the running pass to stop after the title it is on.
    stop_producing: std::sync::atomic::AtomicBool,
    /// A genre-backfill pass is running. Same reasoning as `producing`: the
    /// question is "may another one start", not "wait for this one" — two
    /// passes would read the same cursor, fetch the same titles and double
    /// the request count for nothing.
    backfilling_genres: std::sync::atomic::AtomicBool,
    /// What the last genre-backfill pass did. Server-wide rather than per
    /// library because the backfill is: it walks item ids, not libraries.
    /// Surfaced on the settings page, which is where an operator armed it.
    last_genre_backfill: Mutex<Option<GenreBackfillReport>>,
}

/// The title a producer pass is working on, and why it chose it.
#[derive(Clone, Debug, Serialize)]
pub struct ProducingNow {
    pub title: String,
    /// One of [`crate::produce::REASON_IN_PROGRESS`] and friends — the rail
    /// this candidate came off. "Why is it encoding *that*" is half the
    /// question, and the answer is never obvious from the filename.
    pub reason: &'static str,
    /// 1-based position within this pass, and how many it means to attempt.
    pub index: usize,
    pub total: usize,
}

/// Clears [`JobManager::backfilling_genres`] however the pass ends, panic
/// included. A flag left set by an early return is a job that never runs
/// again until the process restarts, and this one has no interval to make
/// that visible.
struct GenreBackfillGuard(Arc<JobManager>);

impl Drop for GenreBackfillGuard {
    fn drop(&mut self) {
        self.0.backfilling_genres.store(false, Ordering::Relaxed);
    }
}

/// Clears [`JobManager::producing`] however the pass ends — including the ways
/// a `?` or a panic would leave it set forever, which would silently stop the
/// producer for the life of the process.
struct ProducingGuard(Arc<JobManager>);

impl Drop for ProducingGuard {
    fn drop(&mut self) {
        self.0.producing.store(false, Ordering::Relaxed);
        self.0.stop_producing.store(false, Ordering::Relaxed);
        // The label has to go with the flag. A pass that ends between titles
        // would otherwise leave the activity feed claiming an encode that is
        // not happening — worse than saying nothing, because it is wrong.
        if let Ok(mut now) = self.0.now_producing.try_lock() {
            *now = None;
        }
    }
}

/// How many rows to take off each rail, per user. A rail is a prediction and
/// the tail of one is a weak prediction; the head is where the value is.
const PRODUCE_RAIL: i64 = 5;

/// Ceiling on what one pass will attempt, across every user and rail.
const PRODUCE_MAX_PER_PASS: usize = 12;

/// How long one pass may spend. A bound rather than "until the list is done"
/// because the list is never done: it is regenerated every interval from what
/// people are actually watching, and a pass that ran for a day would be
/// producing yesterday's predictions.
const PRODUCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

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
            #[cfg(test)]
            tmdb_base: None,
            statuses: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(VecDeque::new()),
            metrics: IntegrationMetrics::default(),
            producing: std::sync::atomic::AtomicBool::new(false),
            now_producing: Mutex::new(None),
            stop_producing: std::sync::atomic::AtomicBool::new(false),
            backfilling_genres: std::sync::atomic::AtomicBool::new(false),
            last_genre_backfill: Mutex::new(None),
        }
    }

    /// What the last genre-backfill pass did, if one has run since boot.
    pub async fn last_genre_backfill(&self) -> Option<GenreBackfillReport> {
        self.last_genre_backfill.lock().await.clone()
    }

    /// What the pre-transcode pass is working on, or `None` if none is.
    pub async fn producing_now(&self) -> Option<ProducingNow> {
        self.now_producing.lock().await.clone()
    }

    /// Publish (or clear) the title the pass is on. The pass owns this; it is
    /// crate-visible so the HTTP layer's tests can put the server in the state
    /// an admin actually complains about.
    pub(crate) async fn set_producing(&self, now: Option<ProducingNow>) {
        *self.now_producing.lock().await = now;
    }

    /// Ask a running pass to stop. It finishes the title it is on — killing an
    /// encoder mid-segment would throw away the part it has made, and the
    /// producer is built to resume from published boundaries, not from the
    /// middle of one. Returns whether there was anything to stop.
    pub fn stop_producing(&self) -> bool {
        if !self.producing.load(Ordering::Relaxed) {
            return false;
        }
        self.stop_producing.store(true, Ordering::Relaxed);
        true
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

        // The rows exist; without this they would have no artwork until
        // somebody pressed Scan. Bounded to what this request placed (and the
        // seasons/shows/folders above it) because monarr is holding the HTTP
        // connection open on this call — enriching the whole library here
        // would turn a per-episode import notification into a per-episode
        // full-library metadata pass.
        let placed: Vec<i64> = out.items.iter().map(|p| p.item_id).collect();
        let targets = self.enrich_targets(&placed).await;
        // A new episode often lands under a show that was enriched months
        // ago. The ordinary `force = false` queue quite correctly omits that
        // show, but episode/season enrichment is reached *through* the show,
        // so omitting it also strands every newly placed child without art.
        // Force only this bounded TV tree; `only` still prevents a targeted
        // notification from becoming a whole-library refresh.
        let refresh_existing_show = library.kind == LibraryKind::Shows && !library.anime;
        let outcome = self
            .enrich(&library, refresh_existing_show, Some(&targets))
            .await;
        tracing::info!(
            target: "plurxd::integrate",
            library = req.library_id,
            items = targets.len(),
            matched = outcome.enrich.map(|r| r.matched).unwrap_or(0),
            correlation_id = req.correlation_id.as_deref().unwrap_or("-"),
            "targeted scan enriched what it placed"
        );
        Ok(out)
    }

    /// Which item ids a targeted scan should enrich, given what it placed.
    ///
    /// Not the placed ids alone: `scan_path` returns the rows that own the
    /// *files*, which for a show are episodes — and episodes are not what
    /// `items_needing_metadata` selects, so filtering on them would enrich
    /// precisely nothing. The identity of an episode lives on its show, and a
    /// home video's poster is what its folder inherits, so every ancestor
    /// comes along too.
    async fn enrich_targets(&self, item_ids: &[i64]) -> Vec<i64> {
        let mut targets: Vec<i64> = Vec::new();
        for id in item_ids {
            let mut current = *id;
            // Depth guard, not a shape assumption: library → show → season →
            // episode is three levels, home folders can nest deeper, and a
            // cycle in parent_id would otherwise hang the request.
            for _ in 0..8 {
                if !targets.contains(&current) {
                    targets.push(current);
                }
                match self.store.get_item(current).await {
                    Ok(Some(item)) => match item.parent_id {
                        Some(parent) => current = parent,
                        None => break,
                    },
                    // A row that vanished between the scan and here is not
                    // worth failing the request over; it simply gets no
                    // enrichment, exactly as if it had not been placed.
                    _ => break,
                }
            }
        }
        targets
    }

    /// The one enrichment path. Both scans call it; neither has its own copy.
    ///
    /// It is a method rather than two blocks because it used to be two
    /// blocks — or rather one, in `run_scan`, with `run_targeted` silently
    /// having none. Peer-ingested items got a row and never got artwork, and
    /// nothing about either function's shape said they were supposed to
    /// agree. Now they physically cannot drift apart: there is one place that
    /// knows a home library enriches locally, an anime library from AniList,
    /// and everything else from TMDB, and adding a fourth kind is one edit.
    ///
    /// `force` re-fetches already-matched items; `only` narrows to specific
    /// item ids (`None` = the whole library, the full scan's behaviour,
    /// unchanged).
    async fn enrich(&self, library: &Library, force: bool, only: Option<&[i64]>) -> EnrichOutcome {
        let mut outcome = EnrichOutcome::default();
        // Home libraries have no provider at all: their enrichment is local
        // artwork (frame grabs and adopted sidecar images). Anime libraries
        // enrich from AniList (no key needed); everything else from TMDB when
        // a key is configured.
        if library.kind == LibraryKind::Home {
            outcome.local_art = Some(
                metadata::local::enrich_home_library(
                    self.store.as_ref(),
                    &self.artwork_dir,
                    library.id,
                    force,
                    only,
                )
                .await,
            );
        } else if library.anime {
            let client = AniListClient::new();
            outcome.enrich = Some(
                metadata::enrich_anime_library(
                    self.store.as_ref(),
                    &client,
                    &self.artwork_dir,
                    library.id,
                    force,
                    only,
                )
                .await,
            );
        } else {
            match self.store.get_setting(keys::TMDB_API_KEY).await {
                Ok(Some(key)) if !key.is_empty() => {
                    let tmdb = self.tmdb_client(key);
                    outcome.enrich = Some(
                        metadata::enrich_library(
                            self.store.as_ref(),
                            &tmdb,
                            &self.artwork_dir,
                            Some(library.id),
                            force,
                            only,
                        )
                        .await,
                    );
                }
                Ok(_) => tracing::info!("no TMDB key configured; skipping enrichment"),
                Err(e) => tracing::warn!(error = %e, "reading TMDB key"),
            }
        }
        outcome
    }

    fn tmdb_client(&self, key: String) -> TmdbClient {
        #[cfg(test)]
        if let Some((base, image_base)) = &self.tmdb_base {
            return TmdbClient::new(key.as_str()).with_base(base, image_base);
        }
        TmdbClient::new(key)
    }

    /// Re-fetch artwork for one item and its ancestors, ignoring every "we
    /// already did this" marker. The per-item counterpart to a library
    /// refresh, for when a poster is wrong or missing on exactly one thing.
    pub async fn refresh_item_artwork(&self, item_id: i64) -> Result<EnrichOutcome, StoreError> {
        let Some(item) = self.store.get_item(item_id).await? else {
            return Ok(EnrichOutcome::default());
        };
        let Some(library) = self.store.get_library(item.library_id).await? else {
            return Ok(EnrichOutcome::default());
        };
        // Ancestors for the same reason a targeted scan needs them: an
        // episode's artwork is fetched through its show's season, so asking
        // for the episode alone would ask for nothing.
        let targets = self.enrich_targets(&[item_id]).await;
        Ok(self.enrich(&library, true, Some(&targets)).await)
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

        // `None`: the whole library, which is what a full scan means. The
        // provider-choosing lives in `enrich` so the targeted path cannot
        // have a different idea of it.
        let outcome = self.enrich(&library, force_metadata, None).await;
        status.last_enrich = outcome.enrich;
        status.last_local_art = outcome.local_art;

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
            // The one job whose absent setting is not "off". A default of 0
            // here would ship the exact bug this job exists to fix: every
            // item whose poster download failed would sit there with a blank
            // card until someone found a button to press, which is the state
            // that made the job necessary in the first place.
            artwork_retry_mins: self
                .job_interval_or(
                    keys::JOB_ARTWORK_RETRY_MINS,
                    keys::ARTWORK_RETRY_DEFAULT_MINS,
                )
                .await,
            last_artwork_retry: self.job_stamp(keys::JOB_LAST_ARTWORK_RETRY).await,
            transcode_cleanup_mins: self.job_interval(keys::JOB_TRANSCODE_CLEANUP_MINS).await,
            last_transcode_cleanup: self.job_stamp(keys::JOB_LAST_TRANSCODE_CLEANUP).await,
            cache_produce_mins: self.job_interval(keys::JOB_CACHE_PRODUCE_MINS).await,
            last_cache_produce: self.job_stamp(keys::JOB_LAST_CACHE_PRODUCE).await,
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
                DueJob::RetryArtwork => {
                    self.stamp(keys::JOB_LAST_ARTWORK_RETRY).await;
                    self.sweep_artwork().await?;
                }
                DueJob::CleanupTranscode => {
                    self.stamp(keys::JOB_LAST_TRANSCODE_CLEANUP).await;
                    let removed = transcode.sweep_orphan_dirs().await;
                    if removed > 0 {
                        tracing::info!(removed, "swept orphaned transcode directories");
                    }
                    // The cache is swept here as well as before each producer
                    // run, and the redundancy is the point: production and
                    // eviction are different settings, and a server whose
                    // producer was turned off after filling the cache still has
                    // to be able to get its disk back.
                    if let Some((root, node)) = transcode.cache_location() {
                        crate::cachekeep::sweep_with_readers(
                            &self.store,
                            root,
                            node,
                            transcode.cache_readers(),
                            now(),
                        )
                        .await;
                    }
                }
                DueJob::ProduceCache => {
                    self.stamp(keys::JOB_LAST_CACHE_PRODUCE).await;
                    // Spawned rather than run inline: this one takes hours, and
                    // the scheduler tick it is on is also what starts scans and
                    // sweeps. `run_due_jobs` is stamped-before-run, so a
                    // producer still going when the next tick arrives simply
                    // finds itself already stamped and does not double up —
                    // and `produce_pass` refuses a second concurrent run
                    // outright.
                    let state = Arc::clone(self);
                    let transcode = Arc::clone(transcode);
                    tokio::spawn(async move { state.produce_pass(transcode).await });
                }
            }
        }

        // Not a `DueJob`: there is no interval to decide about. It runs on
        // every tick while it is armed and stops by disarming itself, which
        // is the whole of its schedule — putting that through `due_jobs`
        // would be a scheduling decision that isn't one.
        if metadata::genres::is_armed(self.store.as_ref()).await {
            let state = Arc::clone(self);
            tokio::spawn(async move { state.genre_backfill_pass().await });
        }
        Ok(())
    }

    /// One paced, resumable pass of the genre backfill (S3).
    ///
    /// Spawned rather than run inline for the reason the producer is: a pass
    /// is up to [`metadata::genres::BATCH`] paced provider calls, and the tick
    /// it would otherwise block is what starts scans and sweeps. The guard is
    /// what makes "every tick" safe — a pass still going when the next minute
    /// arrives is not joined by a second one reading the same cursor.
    async fn genre_backfill_pass(self: Arc<Self>) {
        if self.backfilling_genres.swap(true, Ordering::Relaxed) {
            tracing::debug!("a genre backfill pass is already running; skipping this one");
            return;
        }
        let _guard = GenreBackfillGuard(Arc::clone(&self));

        // No key configured is not a reason to skip the pass: anime libraries
        // enrich from AniList, which needs none, and those titles are exactly
        // as entitled to genres as the rest.
        let tmdb = match self.store.get_setting(keys::TMDB_API_KEY).await {
            Ok(Some(key)) if !key.is_empty() => Some(TmdbClient::new(key)),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "genre backfill: reading TMDB key");
                None
            }
        };
        let anilist = AniListClient::new();
        let report = metadata::genres::backfill_pass(
            self.store.as_ref(),
            tmdb.as_ref(),
            &anilist,
            metadata::genres::PACE,
        )
        .await;
        if let Some(report) = report {
            for problem in &report.problems {
                tracing::error!(problem = %problem, "genre backfill problem");
            }
            *self.last_genre_backfill.lock().await = Some(report);
        }
    }

    /// One producer pass: sweep the cache back under budget, work out what
    /// somebody is likely to play next, and pre-transcode as much of it as the
    /// window allows (PERF-PLAN §6.2).
    ///
    /// Everything here is best-effort by construction. A candidate that fails
    /// is logged and skipped rather than ending the pass: the list is a
    /// prediction, and one bad prediction is not a reason to stop making them.
    async fn produce_pass(self: Arc<Self>, transcode: Arc<TranscodeManager>) {
        use crate::produce;
        let Some((root, node)) = transcode.cache_location() else {
            return;
        };
        // One at a time. Two passes would fight for the same slots and the
        // same claims — the claims would sort it out correctly, but only after
        // both had spawned encoders.
        if self.producing.swap(true, Ordering::Relaxed) {
            tracing::debug!("a producer pass is already running; skipping this one");
            return;
        }
        let _running = ProducingGuard(Arc::clone(&self));

        // Under budget BEFORE producing, not after. Producing first would push
        // the cache over its ceiling and then evict — and the eviction is LRU,
        // so what it takes could easily be the entry just made.
        crate::cachekeep::sweep_with_readers(
            &self.store,
            root,
            node,
            transcode.cache_readers(),
            now(),
        )
        .await;
        if crate::cachekeep::budget_bytes(&self.store).await.is_none() {
            tracing::debug!("cache is switched off; nothing to produce");
            return;
        }

        let users = match self.store.list_users().await {
            Ok(users) => users,
            Err(e) => {
                tracing::warn!(error = %e, "producer: cannot list users");
                return;
            }
        };
        let mut rails: Vec<Vec<produce::Candidate>> = Vec::new();
        let mut in_progress = Vec::new();
        let mut next_up = Vec::new();
        for user in &users {
            if let Ok(rows) = self.store.continue_watching(user.id, PRODUCE_RAIL).await {
                in_progress.extend(rows.into_iter().map(|r| (r.item.id, r.item.title)));
            }
            if let Ok(rows) = self.store.next_up(user.id, PRODUCE_RAIL).await {
                next_up.extend(rows.into_iter().map(|r| (r.item.id, r.item.title)));
            }
        }
        // The fallback rail, and the only one a brand-new server has: nobody
        // has watch history on day one, but a 4K film that landed yesterday is
        // still the most likely thing to be played tonight.
        let recent: Vec<(i64, String)> = self
            .store
            .recently_added(None, PRODUCE_RAIL)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.item.id, r.item.title))
            .collect();

        // Resolve items to their files here rather than in `rank`, so the
        // ranking stays a pure list operation and an item with no playable
        // file simply never enters it.
        for (reason, items) in [
            (produce::REASON_IN_PROGRESS, in_progress),
            (produce::REASON_NEXT_UP, next_up),
            (produce::REASON_RECENT, recent),
        ] {
            let mut rail = Vec::new();
            for (item_id, title) in items {
                if let Ok(files) = self.store.files_for_item(item_id).await {
                    if let Some(f) = files.first() {
                        rail.push(produce::Candidate {
                            file_id: f.id,
                            item_id,
                            title,
                            reason,
                        });
                    }
                }
            }
            rails.push(rail);
        }

        let candidates = produce::rank(&rails, PRODUCE_MAX_PER_PASS);
        if candidates.is_empty() {
            return;
        }
        let deadline = std::time::Instant::now() + PRODUCE_WINDOW;
        tracing::info!(
            candidates = candidates.len(),
            window_mins = PRODUCE_WINDOW.as_secs() / 60,
            "pre-transcode pass starting"
        );
        let total = candidates.len();
        for (i, c) in candidates.into_iter().enumerate() {
            if std::time::Instant::now() >= deadline {
                tracing::info!("pre-transcode pass out of time");
                break;
            }
            if self.stop_producing.load(Ordering::Relaxed) {
                tracing::info!("pre-transcode pass stopped by request");
                break;
            }
            let Ok(Some(file)) = self.store.get_file(c.file_id).await else {
                continue;
            };
            if !produce::worth_producing(&file) {
                continue;
            }
            // Published BEFORE the encode starts, not after it finishes. The
            // encode is the part that takes hours and holds the hardware, so
            // announcing it afterwards describes only work that is already
            // over — which is exactly the gap that made a busy ffmpeg
            // unattributable from inside the product.
            self.set_producing(Some(ProducingNow {
                title: c.title.clone(),
                reason: c.reason,
                index: i + 1,
                total,
            }))
            .await;
            tracing::info!(
                title = %c.title, reason = c.reason, n = i + 1, of = total,
                "pre-transcoding"
            );
            // The rung a viewer would actually be given, so the entry matches
            // what a real playback looks up. Asking the manager rather than
            // assuming is what keeps the two in step when Auto's policy moves.
            let height = transcode.auto_height(file.height).await;
            match transcode.produce(&file, height, deadline).await {
                Ok(Some(made)) => {
                    tracing::info!(
                        recipe = %made.recipe, title = %c.title, reason = c.reason, height,
                        minutes = made.duration_ms / 60_000, segments = made.segments,
                        mb = made.bytes / 1_048_576, parts = made.parts,
                        "pre-transcoded"
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(title = %c.title, error = %e, "pre-transcode failed");
                }
            }
            self.set_producing(None).await;
        }
    }

    /// Give every enriched item that still has no poster another go.
    ///
    /// The self-healing half of the artwork fix: §2 records *that* a download
    /// failed, this is what comes back for it. Forced, because these items
    /// already carry `metadata_at` and the ordinary queue would skip them
    /// forever — which is precisely how they got here.
    ///
    /// Grouped by library because that is the unit that knows which provider
    /// to ask. The per-item backoff, not this interval, is what stops a
    /// permanently art-less item from being re-fetched every half hour.
    pub async fn sweep_artwork(&self) -> Result<usize, plurx_core::error::StoreError> {
        let items = self
            .store
            .items_missing_artwork(None, keys::ARTWORK_RETRY_BACKOFF_SECS)
            .await?;
        if items.is_empty() {
            return Ok(0);
        }
        let mut by_library: HashMap<i64, Vec<i64>> = HashMap::new();
        for item in &items {
            by_library.entry(item.library_id).or_default().push(item.id);
        }
        let mut repaired = 0usize;
        for (library_id, ids) in by_library {
            let Ok(Some(library)) = self.store.get_library(library_id).await else {
                continue;
            };
            // Metadata providers enter a TV tree through the show, never
            // through a season/episode row. Carry each missing child and all
            // of its ancestors so `enrich_library` selects the show while
            // `enrich_episodes` remains narrowed to exactly the affected
            // seasons and episodes.
            let targets = self.enrich_targets(&ids).await;
            let outcome = self.enrich(&library, true, Some(&targets)).await;
            repaired += outcome.enrich.map(|r| r.matched).unwrap_or(0);
        }
        tracing::info!(
            attempted = items.len(),
            repaired,
            "artwork retry sweep finished"
        );
        Ok(repaired)
    }

    /// A minutes-interval setting; absent, blank or unparseable reads as off.
    /// Deliberately not an error: a hand-edited settings row should stop one
    /// job, not the server.
    async fn job_interval(&self, key: &str) -> i64 {
        self.job_interval_or(key, 0).await
    }

    /// [`job_interval`](Self::job_interval) with a different reading of
    /// "absent". A stored `0` still means off — an admin who turned a job off
    /// must not have it turned back on by a default.
    async fn job_interval_or(&self, key: &str, default_mins: i64) -> i64 {
        self.store
            .get_setting(key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(default_mins)
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

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::domain::{ItemKind, NewItem, NewLibrary};
    use plurx_core::store::{LibraryStore, MediaStore, SettingsStore, SqliteStore};
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    fn manager(store: Arc<dyn Store>, artwork: &std::path::Path) -> Arc<JobManager> {
        Arc::new(JobManager::new(store, artwork.to_path_buf()))
    }

    fn manager_with_tmdb(
        store: Arc<dyn Store>,
        artwork: &std::path::Path,
        base: &str,
    ) -> Arc<JobManager> {
        let mut manager = JobManager::new(store, artwork.to_path_buf());
        manager.tmdb_base = Some((base.to_owned(), base.to_owned()));
        Arc::new(manager)
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn targeted_show_tmdb(season_one_hits: Arc<AtomicUsize>) -> axum::Router {
        use axum::routing::get;
        use axum::Json;

        axum::Router::new()
            .route(
                "/tv/42",
                get(|| async {
                    Json(json!({
                        "id": 42,
                        "name": "Severance",
                        "first_air_date": "2022-02-18",
                        "poster_path": "/show.jpg",
                        "backdrop_path": "/backdrop.jpg"
                    }))
                }),
            )
            .route(
                "/tv/42/season/1",
                get(move || {
                    let hits = Arc::clone(&season_one_hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "poster_path": "/season-1.jpg",
                            "episodes": [{
                                "episode_number": 1,
                                "name": "Good News About Hell",
                                "still_path": "/episode-1.jpg"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/tv/42/season/2",
                get(|| async {
                    Json(json!({
                        "poster_path": "/season-2.jpg",
                        "episodes": [{
                            "episode_number": 1,
                            "name": "Hello, Ms. Cobel",
                            "still_path": "/episode-2.jpg"
                        }]
                    }))
                }),
            )
            // Image paths are served by the same base in this test.
            .fallback(get(|| async { vec![0_u8, 1, 2, 3] }))
    }

    /// The bug this whole change exists for, in one test.
    ///
    /// monarr POSTs `/api/v1/scan` the moment an import finishes and waits for
    /// the answer. Before this, the handler placed the row and stopped — no
    /// enrichment, no artwork — and the item sat with a blank card until
    /// somebody pressed Scan on the whole library. A home library is the
    /// provider-free way to prove it: its enrichment adopts the picture
    /// already sitting next to the file, so the assertion is about *whether
    /// enrichment ran at all*, not about anyone's network.
    #[tokio::test]
    async fn a_targeted_scan_enriches_what_it_placed() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let artwork = tempfile::tempdir().expect("artwork");
        std::fs::create_dir_all(media.path().join("Holiday")).expect("mkdir");
        std::fs::write(media.path().join("Holiday/clip.mp4"), b"not really video").expect("clip");
        std::fs::write(media.path().join("Holiday/clip-thumb.jpg"), b"jpeg-ish").expect("thumb");

        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![media.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");

        let jobs = manager(store.clone(), artwork.path());
        let scan = jobs
            .request_scan(ScanRequest {
                id: "req-1".into(),
                library_id: lib.id,
                path: media.path().join("Holiday"),
                ids: None,
                correlation_id: None,
                source: Some("monarr".into()),
            })
            .await
            .expect("scan ran")
            .expect("not queued");
        assert_eq!(scan.items.len(), 1, "the clip was placed");

        let clip = store
            .get_item(scan.items[0].item_id)
            .await
            .expect("get")
            .expect("item");
        assert!(
            clip.poster_path.is_some(),
            "a peer-ingested item must come out of the targeted scan with \
             artwork; before the fix this was None and stayed None"
        );

        // And the folder above it inherited that poster — which only happens
        // if the ancestors were enriched too, not just the placed row.
        let folder = store
            .get_item(clip.parent_id.expect("parent"))
            .await
            .expect("get")
            .expect("folder");
        assert_eq!(folder.kind, ItemKind::Folder);
        assert!(folder.poster_path.is_some());
    }

    /// A show can be old while the episode is brand new. The show row is the
    /// gateway to TMDB's season endpoint, so treating its earlier enrichment
    /// stamp as a reason to skip it strands the new season and episode with
    /// blank cards. This is the shape seen in the live library: movies from
    /// the same import window had posters, episodes under known shows did not.
    #[tokio::test]
    async fn a_targeted_scan_enriches_new_children_of_an_existing_show() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let media = tempfile::tempdir().expect("media");
        let artwork = tempfile::tempdir().expect("artwork");
        let show = media.path().join("Severance (2022)");
        let season_one = show.join("Season 01");
        std::fs::create_dir_all(&season_one).expect("mkdir s1");
        std::fs::write(season_one.join("Severance.S01E01.mkv"), b"video").expect("episode 1");

        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![media.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        store
            .put_setting(keys::TMDB_API_KEY, "test-key")
            .await
            .expect("key");

        let season_one_hits = Arc::new(AtomicUsize::new(0));
        let base = serve(targeted_show_tmdb(Arc::clone(&season_one_hits))).await;
        let jobs = manager_with_tmdb(store.clone(), artwork.path(), &base);
        let ids = Some(IdHints {
            series_tmdb: Some(42),
            episodeish: true,
            ..Default::default()
        });

        let first = jobs
            .request_scan(ScanRequest {
                id: "s1".into(),
                library_id: lib.id,
                path: season_one,
                ids: ids.clone(),
                correlation_id: None,
                source: Some("monarr".into()),
            })
            .await
            .expect("first scan")
            .expect("first ran");
        let first_episode = store
            .get_item(first.items[0].item_id)
            .await
            .expect("get first")
            .expect("first episode");
        assert!(first_episode.poster_path.is_some(), "initial episode art");

        // The show is now metadata-stamped. A later notification for a new
        // season must still walk through it to hydrate the newly placed rows.
        let season_two = show.join("Season 02");
        std::fs::create_dir_all(&season_two).expect("mkdir s2");
        std::fs::write(season_two.join("Severance.S02E01.mkv"), b"video").expect("episode 2");
        let second = jobs
            .request_scan(ScanRequest {
                id: "s2".into(),
                library_id: lib.id,
                path: season_two,
                ids,
                correlation_id: None,
                source: Some("monarr".into()),
            })
            .await
            .expect("second scan")
            .expect("second ran");
        let second_episode = store
            .get_item(second.items[0].item_id)
            .await
            .expect("get second")
            .expect("second episode");
        assert!(
            second_episode.poster_path.is_some(),
            "a new episode under an existing show must leave the notification path with artwork"
        );
        let second_season = store
            .get_item(second_episode.parent_id.expect("season"))
            .await
            .expect("get season")
            .expect("second season");
        assert!(second_season.poster_path.is_some(), "new season poster");
        assert_eq!(
            season_one_hits.load(Ordering::SeqCst),
            1,
            "the targeted retry must not re-download every existing season"
        );
    }

    /// `scan_path` hands back the rows that own the *files* — episodes, not
    /// shows. Enriching those ids alone would enrich nothing, because a show
    /// is what the provider is asked about.
    #[tokio::test]
    async fn enrich_targets_walks_up_to_the_show() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let artwork = tempfile::tempdir().expect("artwork");
        let lib = store
            .create_library(&NewLibrary {
                name: "Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let new = |kind: ItemKind, parent: Option<i64>, title: &str| {
            let store = store.clone();
            let item = NewItem {
                library_id: lib.id,
                kind,
                parent_id: parent,
                title: title.into(),
                year: None,
                season_number: None,
                episode_number: None,
            };
            async move { store.insert_item(&item).await.expect("insert") }
        };
        let show = new(ItemKind::Show, None, "Severance").await;
        let season = new(ItemKind::Season, Some(show), "S1").await;
        let episode = new(ItemKind::Episode, Some(season), "E1").await;

        let jobs = manager(store.clone(), artwork.path());
        let targets = jobs.enrich_targets(&[episode]).await;
        assert!(targets.contains(&show), "the identity lives on the show");
        assert!(targets.contains(&season));
        assert!(targets.contains(&episode));

        // Two episodes of the same show contribute the show once, not twice —
        // a season import must not enrich the show ten times over.
        let episode2 = new(ItemKind::Episode, Some(season), "E2").await;
        let targets = jobs.enrich_targets(&[episode, episode2]).await;
        assert_eq!(targets.iter().filter(|id| **id == show).count(), 1);
    }

    /// The sweep with nothing to sweep must not invent work — and must not
    /// need a provider key to say so.
    #[tokio::test]
    async fn the_artwork_sweep_is_a_no_op_on_a_healthy_library() {
        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let artwork = tempfile::tempdir().expect("artwork");
        let jobs = manager(store.clone(), artwork.path());
        assert_eq!(jobs.sweep_artwork().await.expect("sweep"), 0);
    }

    /// Production shape: the known show has a poster and an enrichment stamp,
    /// while a later-imported season and episode have neither artwork nor an
    /// attempt stamp. The sweep must enter through the show and repair both
    /// child cards; selecting the child ids without their ancestor enriches
    /// nothing.
    #[tokio::test]
    async fn the_artwork_sweep_repairs_blank_tv_children() {
        use plurx_core::domain::{ArtworkAttempt, MetadataPatch};

        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let artwork = tempfile::tempdir().expect("artwork");
        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        store
            .put_setting(keys::TMDB_API_KEY, "test-key")
            .await
            .expect("key");
        let show = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Severance".into(),
                year: Some(2022),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let episode = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "Episode 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("episode");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    tmdb_id: Some(42),
                    poster_path: Some("existing-show.jpg".into()),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Stored),
                    ..Default::default()
                },
            )
            .await
            .expect("enrich show");

        let season_hits = Arc::new(AtomicUsize::new(0));
        let base = serve(targeted_show_tmdb(Arc::clone(&season_hits))).await;
        let jobs = manager_with_tmdb(store.clone(), artwork.path(), &base);
        assert_eq!(jobs.sweep_artwork().await.expect("sweep"), 1);

        let season = store
            .get_item(season)
            .await
            .expect("get season")
            .expect("season");
        let episode = store
            .get_item(episode)
            .await
            .expect("get episode")
            .expect("episode");
        assert!(season.poster_path.is_some(), "season card repaired");
        assert!(episode.poster_path.is_some(), "episode card repaired");
        assert_eq!(
            season_hits.load(Ordering::SeqCst),
            1,
            "one affected season costs one TMDB season request"
        );
    }
}
