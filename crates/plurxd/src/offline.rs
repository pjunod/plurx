//! Durable offline-package preparation coordinator.
//!
//! JSON handlers only create intent. This loop owns the long-running work so
//! an HTTP disconnect or daemon restart cannot abandon a traveller's queue.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{collections::HashMap, time::SystemTime};

use plurx_core::domain::OfflinePackage;
use plurx_core::store::{keys, Store};

use crate::transcode::{
    OfflineProduceOutcome, OfflineSpec, OfflineSubtitle, Produced, TranscodeManager,
};

const IDLE_POLL: Duration = Duration::from_secs(2);
const PRODUCE_PASS: Duration = Duration::from_secs(6 * 60 * 60);
const DURATION_TOLERANCE_MS: i64 = 10_000;
const TRANSFER_ACTIVE_FOR: Duration = Duration::from_secs(20);
const TRANSFER_SAMPLE_TTL: Duration = Duration::from_secs(2 * 60);
const QUALITY_HEIGHTS: [i64; 4] = [360, 480, 720, 1080];
const PREPARE_RESULTS: [&str; 3] = ["ok", "failed", "cancelled"];
const PREPARE_BUCKETS: [u64; 7] = [60, 300, 900, 3_600, 10_800, 21_600, u64::MAX];
const FAILURE_CODES: [&str; 5] = [
    "source_unavailable",
    "invalid_track",
    "encoder_failed",
    "subtitle_failed",
    "other",
];
const QUOTA_REASONS: [&str; 3] = ["registry", "user_bytes", "global_bytes"];

#[derive(Clone, Copy)]
pub(crate) enum OfflineQuota {
    Registry,
    UserBytes,
    GlobalBytes,
}

impl OfflineQuota {
    fn index(self) -> usize {
        match self {
            Self::Registry => 0,
            Self::UserBytes => 1,
            Self::GlobalBytes => 2,
        }
    }
}

#[derive(Clone, Copy)]
struct TransferSample {
    bytes: u64,
    last_seen: Instant,
}

/// The one owner of bounded offline counters and preparation histograms.
///
/// Prometheus history is intentionally process-local, like the existing scan
/// counters. A scraper handles resets; durable package state supplies gauges.
/// Arrays encode every permitted label value so request data can never create
/// a new series.
struct OfflineMetrics {
    requests: [AtomicU64; 4],
    quota_rejections: [AtomicU64; 3],
    prepare_count: [AtomicU64; 3],
    prepare_seconds: [AtomicU64; 3],
    prepare_buckets: [[AtomicU64; 7]; 3],
    prepare_work_millis: [AtomicU64; 3],
    partial_work_millis: Mutex<HashMap<String, u64>>,
    prepared_media_seconds: AtomicU64,
    prepared_bytes: AtomicU64,
    transfer_bytes: AtomicU64,
    failures: [AtomicU64; 5],
    cancellations: AtomicU64,
    transfers: Mutex<HashMap<String, TransferSample>>,
}

impl OfflineMetrics {
    fn new() -> Self {
        Self {
            requests: std::array::from_fn(|_| AtomicU64::new(0)),
            quota_rejections: std::array::from_fn(|_| AtomicU64::new(0)),
            prepare_count: std::array::from_fn(|_| AtomicU64::new(0)),
            prepare_seconds: std::array::from_fn(|_| AtomicU64::new(0)),
            prepare_buckets: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            prepare_work_millis: std::array::from_fn(|_| AtomicU64::new(0)),
            partial_work_millis: Mutex::new(HashMap::new()),
            prepared_media_seconds: AtomicU64::new(0),
            prepared_bytes: AtomicU64::new(0),
            transfer_bytes: AtomicU64::new(0),
            failures: std::array::from_fn(|_| AtomicU64::new(0)),
            cancellations: AtomicU64::new(0),
            transfers: Mutex::new(HashMap::new()),
        }
    }

    fn record_request(&self, height: i64) {
        if let Some(index) = QUALITY_HEIGHTS
            .iter()
            .position(|candidate| *candidate == height)
        {
            self.requests[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_quota_rejection(&self, quota: OfflineQuota) {
        self.quota_rejections[quota.index()].fetch_add(1, Ordering::Relaxed);
    }

    fn record_prepare(&self, result: usize, package: &OfflinePackage) {
        let seconds = elapsed_since(package.created_at);
        self.prepare_count[result].fetch_add(1, Ordering::Relaxed);
        self.prepare_seconds[result].fetch_add(seconds, Ordering::Relaxed);
        let bucket = PREPARE_BUCKETS
            .iter()
            .position(|upper| seconds <= *upper)
            .unwrap_or(PREPARE_BUCKETS.len() - 1);
        self.prepare_buckets[result][bucket].fetch_add(1, Ordering::Relaxed);
    }

    fn accumulate_work(&self, package_id: &str, elapsed: Duration) {
        let millis = elapsed.as_millis().min(u64::MAX as u128) as u64;
        let mut partial = self
            .partial_work_millis
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total = partial.entry(package_id.to_owned()).or_default();
        *total = total.saturating_add(millis);
    }

    fn finish_work(&self, result: usize, package_id: &str, elapsed: Duration) {
        let current = elapsed.as_millis().min(u64::MAX as u128) as u64;
        let prior = self
            .partial_work_millis
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(package_id)
            .unwrap_or(0);
        self.prepare_work_millis[result]
            .fetch_add(prior.saturating_add(current), Ordering::Relaxed);
    }

    fn forget_work(&self, package_id: &str) {
        self.partial_work_millis
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(package_id);
    }

    fn record_ready(&self, package: &OfflinePackage, media_ms: i64, bytes: i64, elapsed: Duration) {
        self.record_prepare(0, package);
        self.finish_work(0, &package.id, elapsed);
        self.prepared_media_seconds
            .fetch_add((media_ms.max(0) as u64) / 1_000, Ordering::Relaxed);
        self.prepared_bytes
            .fetch_add(bytes.max(0) as u64, Ordering::Relaxed);
    }

    fn record_failure(&self, package: &OfflinePackage, code: &str, elapsed: Duration) {
        self.record_prepare(1, package);
        self.finish_work(1, &package.id, elapsed);
        let index = FAILURE_CODES
            .iter()
            .position(|known| *known == code)
            .unwrap_or(FAILURE_CODES.len() - 1);
        self.failures[index].fetch_add(1, Ordering::Relaxed);
    }

    fn record_cancellation(&self, package: &OfflinePackage) {
        self.cancellations.fetch_add(1, Ordering::Relaxed);
        if matches!(package.state.as_str(), "queued" | "preparing") {
            self.record_prepare(2, package);
            self.finish_work(2, &package.id, Duration::ZERO);
        }
    }

    fn record_transfer(&self, package_id: &str, bytes: usize) {
        let bytes = bytes as u64;
        self.transfer_bytes.fetch_add(bytes, Ordering::Relaxed);
        let now = Instant::now();
        let mut transfers = self
            .transfers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        transfers.retain(|_, sample| now.duration_since(sample.last_seen) <= TRANSFER_SAMPLE_TTL);
        let sample = transfers
            .entry(package_id.to_owned())
            .or_insert(TransferSample {
                bytes: 0,
                last_seen: now,
            });
        sample.bytes = sample.bytes.saturating_add(bytes);
        sample.last_seen = now;
    }

    fn transfer_bytes(&self, package_id: &str) -> Option<u64> {
        let transfers = self
            .transfers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        transfers.get(package_id).and_then(|sample| {
            (sample.last_seen.elapsed() <= TRANSFER_ACTIVE_FOR).then_some(sample.bytes)
        })
    }

    fn forget_transfer(&self, package_id: &str) {
        self.transfers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(package_id);
    }

    fn prometheus(&self) -> String {
        let mut out = String::from(
            "# HELP plurx_offline_requests_total New offline packages accepted, by quality.\n\
             # TYPE plurx_offline_requests_total counter\n",
        );
        for (index, height) in QUALITY_HEIGHTS.iter().enumerate() {
            let _ = writeln!(
                out,
                "plurx_offline_requests_total{{height=\"{height}\"}} {}",
                self.requests[index].load(Ordering::Relaxed)
            );
        }
        out.push_str(
            "# HELP plurx_offline_quota_rejections_total Offline requests refused by bounded quota class.\n\
             # TYPE plurx_offline_quota_rejections_total counter\n",
        );
        for (index, reason) in QUOTA_REASONS.iter().enumerate() {
            let _ = writeln!(
                out,
                "plurx_offline_quota_rejections_total{{reason=\"{reason}\"}} {}",
                self.quota_rejections[index].load(Ordering::Relaxed)
            );
        }
        out.push_str(
            "# HELP plurx_offline_prepare_seconds Time from accepted request to terminal preparation result.\n\
             # TYPE plurx_offline_prepare_seconds histogram\n",
        );
        for (result_index, result) in PREPARE_RESULTS.iter().enumerate() {
            let mut cumulative = 0;
            for (bucket_index, upper) in PREPARE_BUCKETS.iter().enumerate() {
                cumulative +=
                    self.prepare_buckets[result_index][bucket_index].load(Ordering::Relaxed);
                let le = if *upper == u64::MAX {
                    "+Inf".to_owned()
                } else {
                    upper.to_string()
                };
                let _ = writeln!(
                    out,
                    "plurx_offline_prepare_seconds_bucket{{result=\"{result}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "plurx_offline_prepare_seconds_sum{{result=\"{result}\"}} {}",
                self.prepare_seconds[result_index].load(Ordering::Relaxed)
            );
            let _ = writeln!(
                out,
                "plurx_offline_prepare_seconds_count{{result=\"{result}\"}} {}",
                self.prepare_count[result_index].load(Ordering::Relaxed)
            );
        }
        out.push_str(
            "# HELP plurx_offline_prepare_work_seconds_total Active server work spent on terminal offline preparations.\n\
             # TYPE plurx_offline_prepare_work_seconds_total counter\n",
        );
        for (index, result) in PREPARE_RESULTS.iter().enumerate() {
            let seconds = self.prepare_work_millis[index].load(Ordering::Relaxed) as f64 / 1_000.0;
            let _ = writeln!(
                out,
                "plurx_offline_prepare_work_seconds_total{{result=\"{result}\"}} {seconds:.3}"
            );
        }
        let _ = write!(
            out,
            "# HELP plurx_offline_prepared_media_seconds_total Media duration successfully prepared for offline viewing.\n\
             # TYPE plurx_offline_prepared_media_seconds_total counter\n\
             plurx_offline_prepared_media_seconds_total {}\n\
             # HELP plurx_offline_prepared_bytes_total Completed offline package bytes.\n\
             # TYPE plurx_offline_prepared_bytes_total counter\n\
             plurx_offline_prepared_bytes_total {}\n\
             # HELP plurx_offline_transfer_bytes_total Bytes served through offline media leases.\n\
             # TYPE plurx_offline_transfer_bytes_total counter\n\
             plurx_offline_transfer_bytes_total {}\n\
             # HELP plurx_offline_cancellations_total Offline packages cancelled before device completion.\n\
             # TYPE plurx_offline_cancellations_total counter\n\
             plurx_offline_cancellations_total {}\n",
            self.prepared_media_seconds.load(Ordering::Relaxed),
            self.prepared_bytes.load(Ordering::Relaxed),
            self.transfer_bytes.load(Ordering::Relaxed),
            self.cancellations.load(Ordering::Relaxed),
        );
        out.push_str(
            "# HELP plurx_offline_failures_total Offline preparation failures by bounded code.\n\
             # TYPE plurx_offline_failures_total counter\n",
        );
        for (index, code) in FAILURE_CODES.iter().enumerate() {
            let _ = writeln!(
                out,
                "plurx_offline_failures_total{{code=\"{code}\"}} {}",
                self.failures[index].load(Ordering::Relaxed)
            );
        }
        out
    }
}

fn elapsed_since(created_at: i64) -> u64 {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(created_at.max(0) as u64)
}

pub struct OfflineManager {
    store: Arc<dyn Store>,
    transcode: Arc<TranscodeManager>,
    node_id: String,
    active: tokio::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
    metrics: OfflineMetrics,
}

impl OfflineManager {
    pub fn new(
        store: Arc<dyn Store>,
        transcode: Arc<TranscodeManager>,
        node_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            transcode,
            node_id,
            active: tokio::sync::Mutex::new(HashMap::new()),
            metrics: OfflineMetrics::new(),
        })
    }

    pub(crate) fn record_request(&self, height: i64) {
        self.metrics.record_request(height);
    }

    pub(crate) fn record_quota_rejection(&self, quota: OfflineQuota) {
        self.metrics.record_quota_rejection(quota);
    }

    pub(crate) fn record_cancellation(&self, package: &OfflinePackage) {
        self.metrics.record_cancellation(package);
    }

    pub(crate) fn record_transfer(&self, package_id: &str, bytes: usize) {
        self.metrics.record_transfer(package_id, bytes);
    }

    pub(crate) fn transfer_bytes(&self, package_id: &str) -> Option<u64> {
        self.metrics.transfer_bytes(package_id)
    }

    pub(crate) fn forget_transfer(&self, package_id: &str) {
        self.metrics.forget_transfer(package_id);
    }

    pub(crate) fn prometheus(&self) -> String {
        self.metrics.prometheus()
    }

    pub async fn run(self: Arc<Self>) {
        match self
            .store
            .reset_interrupted_offline_packages(&self.node_id)
            .await
        {
            Ok(count) if count > 0 => {
                tracing::info!(count, "requeued interrupted offline packages")
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "could not recover interrupted offline packages")
            }
        }

        let mut next_expiry_sweep = Instant::now();
        loop {
            if Instant::now() >= next_expiry_sweep {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if let Err(error) = self.store.expire_offline_packages(now).await {
                    tracing::warn!(%error, "offline expiry sweep failed");
                }
                next_expiry_sweep = Instant::now() + Duration::from_secs(60);
            }
            if !self.enabled().await {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            let package = match self.store.claim_next_offline_package(&self.node_id).await {
                Ok(Some(package)) => package,
                Ok(None) => {
                    tokio::time::sleep(IDLE_POLL).await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "offline queue lookup failed");
                    tokio::time::sleep(IDLE_POLL).await;
                    continue;
                }
            };
            let cancelled = tokio_util::sync::CancellationToken::new();
            self.active
                .lock()
                .await
                .insert(package.id.clone(), cancelled.clone());
            self.prepare(package.clone(), &cancelled).await;
            self.active.lock().await.remove(&package.id);
        }
    }

    pub async fn cancel(&self, package_id: &str) {
        if let Some(cancelled) = self.active.lock().await.get(package_id).cloned() {
            cancelled.cancel();
        }
    }

    /// Stop every producer promptly when the operator flips the kill switch.
    /// Durable rows remain queued and can resume if the feature is re-enabled.
    pub async fn cancel_all(&self) {
        for cancelled in self.active.lock().await.values() {
            cancelled.cancel();
        }
    }

    async fn enabled(&self) -> bool {
        !matches!(
            self.store
                .get_setting(keys::OFFLINE_ENABLED)
                .await
                .ok()
                .flatten()
                .as_deref(),
            Some("0" | "false" | "off" | "no")
        )
    }

    async fn prepare(
        &self,
        package: OfflinePackage,
        cancelled: &tokio_util::sync::CancellationToken,
    ) {
        let work_started = Instant::now();
        let file = match self.store.get_file(package.file_id).await {
            Ok(Some(file))
                if file.path.to_string_lossy() == package.source_path
                    && file.size == package.source_size
                    && file.mtime == package.source_mtime =>
            {
                file
            }
            Ok(_) => {
                self.fail(
                    &package,
                    "waiting_for_source",
                    "source_unavailable",
                    "The media file is no longer available on the server.",
                    work_started,
                )
                .await;
                return;
            }
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "offline source lookup failed");
                self.requeue(&package, work_started).await;
                return;
            }
        };

        let subtitle = match (package.subtitle_mode.as_str(), package.subtitle_index) {
            ("none", _) => OfflineSubtitle::None,
            ("native", Some(index)) => OfflineSubtitle::Native(index),
            ("burned", Some(index)) => OfflineSubtitle::Burn(index),
            _ => {
                self.fail(
                    &package,
                    "validating",
                    "invalid_track",
                    "The selected subtitle is no longer available.",
                    work_started,
                )
                .await;
                return;
            }
        };
        let Some(effective_rate_control) =
            plurx_core::transcode::EffectiveRateControl::parse_snapshot(
                &package.effective_rate_control,
            )
        else {
            self.fail(
                &package,
                "validating",
                "invalid_rate_control",
                "The download's encoding identity is invalid.",
                work_started,
            )
            .await;
            return;
        };
        let spec = OfflineSpec {
            target_height: package.target_height,
            audio_index: package.audio_index,
            subtitle,
            effective_rate_control,
        };
        let _ = self
            .store
            .update_offline_progress(&package.id, "transcoding", 1)
            .await;
        let outcome = self
            .transcode
            .ensure_offline(
                &package.id,
                &file,
                &spec,
                Instant::now() + PRODUCE_PASS,
                cancelled,
            )
            .await;
        match outcome {
            Ok(OfflineProduceOutcome::Ready(produced))
            | Ok(OfflineProduceOutcome::Cached(produced)) => {
                self.publish_ready(&package, &file, produced, work_started)
                    .await
            }
            Ok(OfflineProduceOutcome::Yielded) | Ok(OfflineProduceOutcome::ClaimedElsewhere) => {
                self.requeue(&package, work_started).await
            }
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "offline preparation failed");
                self.fail(
                    &package,
                    "transcoding",
                    "encoder_failed",
                    "The server could not prepare this download.",
                    work_started,
                )
                .await;
            }
        }
    }

    async fn publish_ready(
        &self,
        package: &OfflinePackage,
        file: &plurx_core::domain::MediaFile,
        produced: Produced,
        work_started: Instant,
    ) {
        let source_duration = file.duration_ms.unwrap_or_default();
        if produced.duration_ms <= 0
            || produced.duration_ms + DURATION_TOLERANCE_MS < source_duration
        {
            self.fail(
                package,
                "publishing",
                "encoder_failed",
                "The prepared package did not contain the complete title.",
                work_started,
            )
            .await;
            return;
        }

        let mut actual_bytes = produced
            .bytes
            .saturating_add(master_playlist(package).len() as i64);
        if package.subtitle_mode == "native" {
            let Some(index) = package.subtitle_index else {
                self.fail(
                    package,
                    "extracting_subtitles",
                    "subtitle_failed",
                    "The selected subtitle could not be included.",
                    work_started,
                )
                .await;
                return;
            };
            let sidecar = crate::subtitles::vtt_path_for_identity(
                self.transcode.subtitle_cache_dir(),
                package.file_id,
                index,
                package.source_size,
                package.source_mtime,
            );
            match tokio::fs::metadata(sidecar).await {
                Ok(metadata) => {
                    actual_bytes = actual_bytes
                        .saturating_add(metadata.len() as i64)
                        .saturating_add(subtitle_playlist(produced.duration_ms).len() as i64);
                }
                Err(error) => {
                    tracing::warn!(package = %package.id, %error, "offline subtitle is missing");
                    self.fail(
                        package,
                        "extracting_subtitles",
                        "subtitle_failed",
                        "The selected subtitle could not be included.",
                        work_started,
                    )
                    .await;
                    return;
                }
            }
        }
        match self
            .store
            .mark_offline_package_ready(
                &package.id,
                &self.node_id,
                &produced.recipe,
                actual_bytes,
                produced.duration_ms,
            )
            .await
        {
            Ok(true) => {
                self.metrics.record_ready(
                    package,
                    produced.duration_ms,
                    actual_bytes,
                    work_started.elapsed(),
                );
                tracing::info!(
                    package = %package.id,
                    recipe = %produced.recipe,
                    bytes = actual_bytes,
                    duration_ms = produced.duration_ms,
                    "offline package ready"
                )
            }
            Ok(false) => tracing::debug!(package = %package.id, "offline package was cancelled"),
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "could not publish offline state")
            }
        }
    }

    async fn requeue(&self, package: &OfflinePackage, work_started: Instant) {
        match self.store.requeue_offline_package(&package.id).await {
            Ok(true) => self
                .metrics
                .accumulate_work(&package.id, work_started.elapsed()),
            Ok(false) => self.metrics.forget_work(&package.id),
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "could not requeue offline package")
            }
        }
        tokio::time::sleep(IDLE_POLL).await;
    }

    async fn fail(
        &self,
        package: &OfflinePackage,
        phase: &str,
        code: &str,
        message: &str,
        work_started: Instant,
    ) {
        match self
            .store
            .fail_offline_package(&package.id, &self.node_id, phase, code, message)
            .await
        {
            Ok(true) => self
                .metrics
                .record_failure(package, code, work_started.elapsed()),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "could not record offline failure")
            }
        }
    }
}

pub fn master_playlist(package: &OfflinePackage) -> String {
    let rung = crate::transcode::ladder(Some(package.target_height))
        .into_iter()
        .find(|rung| rung.height == package.target_height);
    let (peak, average) = rung
        .map(|rung| (rung.peak_kbps * 1000, rung.total_kbps * 1000))
        .unwrap_or((1, 1));
    let subtitles = if package.subtitle_mode == "native" {
        package.subtitle_index.map(|index| {
            let language = package.subtitle_language.as_deref().unwrap_or("und");
            format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Downloaded\",\
                 DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,LANGUAGE=\"{language}\",\
                 URI=\"subs/{index}/index.m3u8\"\n"
            )
        })
    } else {
        None
    };
    let resolution = match (package.output_width, package.output_height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            format!(",RESOLUTION={width}x{height}")
        }
        _ => String::new(),
    };
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n{}#EXT-X-STREAM-INF:BANDWIDTH={peak},\
         AVERAGE-BANDWIDTH={average},CLOSED-CAPTIONS=NONE{}{resolution}\nindex.m3u8\n",
        subtitles.unwrap_or_default(),
        if package.subtitle_mode == "native" {
            ",SUBTITLES=\"subs\""
        } else {
            ""
        },
    )
}

pub fn subtitle_playlist(duration_ms: i64) -> String {
    let target = ((duration_ms.max(1) + 999) / 1000).max(1);
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXTINF:{:.6},\nseg00000.vtt\n#EXT-X-ENDLIST\n",
        duration_ms.max(1) as f64 / 1000.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use plurx_core::domain::{
        ItemKind, LibraryKind, NewItem, NewLibrary, NewOfflinePackage, OfflineCreateOutcome,
        ProbeResult, SubtitleStream,
    };
    use plurx_core::store::SqliteStore;
    use plurx_core::transcode::{EffectiveRateControl, EncoderCaps, Pipeline};

    struct Fixture {
        manager: Arc<OfflineManager>,
        store: Arc<dyn Store>,
        file: plurx_core::domain::MediaFile,
        user_id: i64,
        _root: tempfile::TempDir,
    }

    async fn seeded_fixture() -> Fixture {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("movie.mkv");
        std::fs::write(&source, b"offline lifecycle fixture").expect("source");
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store
            .create_user("traveller", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Offline".into(),
                kind: LibraryKind::Movies,
                paths: vec![root.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("library");
        let item = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Flight".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        let file_id = store
            .upsert_file(
                item,
                &source.to_string_lossy(),
                25,
                7,
                &ProbeResult {
                    duration_ms: Some(90_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(1920),
                    height: Some(1080),
                    subtitle_streams: vec![SubtitleStream {
                        index: 4,
                        codec: "subrip".into(),
                        language: Some("eng".into()),
                        ..Default::default()
                    }],
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        let file = store
            .get_file(file_id)
            .await
            .expect("file lookup")
            .expect("file");
        let transcode = Arc::new(TranscodeManager::new(
            Arc::clone(&store),
            root.path().join("transcode"),
            EncoderCaps::default(),
            Pipeline::Cpu,
        ));
        let manager = OfflineManager::new(Arc::clone(&store), transcode, "test-node".into());
        Fixture {
            manager,
            store,
            file,
            user_id: user.id,
            _root: root,
        }
    }

    async fn claimed_package(
        fixture: &Fixture,
        id: &str,
        subtitle_mode: &str,
        subtitle_index: Option<i64>,
    ) -> OfflinePackage {
        let package = NewOfflinePackage {
            id: id.into(),
            request_id: format!("request-{id}"),
            user_id: fixture.user_id,
            file_id: fixture.file.id,
            node_id: "test-node".into(),
            source_path: fixture.file.path.to_string_lossy().into_owned(),
            source_size: fixture.file.size,
            source_mtime: fixture.file.mtime,
            effective_rate_control: EffectiveRateControl::Vbr.snapshot_value(),
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: None,
            audio_offset_ms: 0,
            subtitle_index,
            subtitle_language: subtitle_index.map(|_| "en".into()),
            subtitle_mode: subtitle_mode.into(),
            estimated_bytes: 100,
            reserved_bytes: 120,
            expires_at: i64::MAX,
        };
        assert!(matches!(
            fixture
                .store
                .create_offline_package(&package, 10, 1_000, 2_000)
                .await
                .expect("create package"),
            OfflineCreateOutcome::Created(_)
        ));
        fixture
            .store
            .claim_next_offline_package("test-node")
            .await
            .expect("claim package")
            .expect("queued package")
    }

    async fn stored_package(fixture: &Fixture, id: &str) -> OfflinePackage {
        fixture
            .store
            .offline_package_for_user(id, fixture.user_id)
            .await
            .expect("package lookup")
            .expect("package")
    }

    fn produced(duration_ms: i64, bytes: i64) -> Produced {
        Produced {
            recipe: "recipe-under-test".into(),
            bytes,
            duration_ms,
            segments: 2,
            parts: 1,
        }
    }

    fn package(mode: &str, index: Option<i64>) -> OfflinePackage {
        OfflinePackage {
            id: "pkg".into(),
            request_id: "request".into(),
            user_id: 1,
            file_id: 2,
            node_id: "node".into(),
            source_path: "/media/movie.mkv".into(),
            source_size: 3,
            source_mtime: 4,
            recipe_hash: None,
            effective_rate_control: "vbr".into(),
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: Some(0),
            audio_offset_ms: 0,
            subtitle_index: index,
            subtitle_language: index.map(|_| "en".to_owned()),
            subtitle_mode: mode.into(),
            state: "preparing".into(),
            phase: "transcoding".into(),
            progress_millis: 0,
            estimated_bytes: 1,
            reserved_bytes: 2,
            actual_bytes: None,
            duration_ms: None,
            error_code: None,
            error_message: None,
            created_at: 0,
            updated_at: 0,
            last_access_at: 0,
            expires_at: 1,
        }
    }

    #[test]
    fn master_advertises_only_the_selected_native_subtitle() {
        let master = master_playlist(&package("native", Some(7)));
        assert!(master.contains("SUBTITLES=\"subs\""), "{master}");
        assert!(master.contains("subs/7/index.m3u8"), "{master}");
        assert!(master.contains("LANGUAGE=\"en\""), "{master}");
        assert!(master.contains("RESOLUTION=1280x720"), "{master}");
        assert!(!master.contains("CODECS="), "{master}");

        let mut plain = package("none", None);
        plain.target_height = 999;
        plain.output_width = None;
        plain.output_height = None;
        let master = master_playlist(&plain);
        assert!(
            master.contains("BANDWIDTH=1,AVERAGE-BANDWIDTH=1"),
            "{master}"
        );
        assert!(!master.contains("SUBTITLES="), "{master}");
        assert!(!master.contains("RESOLUTION="), "{master}");
    }

    #[test]
    fn subtitle_package_is_one_complete_vod_segment() {
        let playlist = subtitle_playlist(90_500);
        assert!(playlist.contains("#EXT-X-TARGETDURATION:91"));
        assert!(playlist.contains("#EXTINF:90.500000"));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn metrics_have_only_the_declared_bounded_labels() {
        let metrics = OfflineMetrics::new();
        let mut ready = package("none", None);
        ready.id = "secret-package-id".into();
        ready.created_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64
            - 120;
        metrics.record_request(720);
        metrics.record_request(999); // An unadvertised height creates no label.
        metrics.record_quota_rejection(OfflineQuota::Registry);
        metrics.record_quota_rejection(OfflineQuota::UserBytes);
        metrics.record_quota_rejection(OfflineQuota::GlobalBytes);
        metrics.accumulate_work(&ready.id, Duration::from_millis(500));
        metrics.record_ready(&ready, 90_000, 400, Duration::from_millis(1_500));
        metrics.record_transfer(&ready.id, 123);

        let mut failed = ready.clone();
        failed.id = "another-secret".into();
        metrics.record_failure(&failed, "unexpected_dynamic_code", Duration::from_secs(3));
        let mut cancelled = ready.clone();
        cancelled.state = "preparing".into();
        metrics.record_cancellation(&cancelled);
        let mut completed = ready.clone();
        completed.id = "completed-package".into();
        completed.state = "ready".into();
        metrics.record_cancellation(&completed);
        metrics.accumulate_work("forgotten-package", Duration::from_millis(50));
        metrics.forget_work("forgotten-package");

        assert_eq!(metrics.transfer_bytes(&ready.id), Some(123));
        let text = metrics.prometheus();
        assert!(text.contains("plurx_offline_requests_total{height=\"720\"} 1"));
        assert!(!text.contains("height=\"999\""));
        assert!(text.contains("plurx_offline_quota_rejections_total{reason=\"global_bytes\"} 1"));
        assert!(text.contains("plurx_offline_prepare_seconds_count{result=\"ok\"} 1"));
        assert!(text.contains("plurx_offline_prepare_seconds_count{result=\"failed\"} 1"));
        assert!(text.contains("plurx_offline_prepare_seconds_count{result=\"cancelled\"} 1"));
        assert!(text.contains("plurx_offline_prepare_work_seconds_total{result=\"ok\"} 2.000"));
        assert!(text.contains("plurx_offline_failures_total{code=\"other\"} 1"));
        assert!(text.contains("plurx_offline_transfer_bytes_total 123"));
        assert!(!text.contains(&ready.id));
        assert!(!text.contains(&failed.id));

        metrics.forget_transfer(&ready.id);
        assert_eq!(metrics.transfer_bytes(&ready.id), None);
    }

    #[tokio::test]
    async fn cancellation_signals_one_or_every_active_preparation() {
        let fixture = seeded_fixture().await;
        let first = tokio_util::sync::CancellationToken::new();
        let second = tokio_util::sync::CancellationToken::new();
        fixture
            .manager
            .active
            .lock()
            .await
            .insert("first".into(), first.clone());
        fixture
            .manager
            .active
            .lock()
            .await
            .insert("second".into(), second.clone());

        fixture.manager.cancel("first").await;
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        fixture.manager.cancel("missing").await;

        fixture.manager.cancel_all().await;
        assert!(second.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn restart_requeues_interrupted_work_even_while_offline_is_disabled() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "restart", "none", None).await;
        assert_eq!(package.state, "preparing");
        fixture
            .store
            .put_setting(keys::OFFLINE_ENABLED, "off")
            .await
            .expect("disable offline");

        let task = tokio::spawn(Arc::clone(&fixture.manager).run());
        for _ in 0..20 {
            tokio::task::yield_now().await;
            if stored_package(&fixture, "restart").await.state == "queued" {
                break;
            }
        }
        assert_eq!(stored_package(&fixture, "restart").await.state, "queued");
        assert!(!fixture.manager.enabled().await);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn queue_marks_a_changed_source_failed_and_keeps_the_typed_reason() {
        let fixture = seeded_fixture().await;
        let mut package = claimed_package(&fixture, "changed-source", "none", None).await;
        assert!(fixture
            .store
            .requeue_offline_package(&package.id)
            .await
            .expect("requeue"));
        package.source_mtime += 1;
        fixture
            .store
            .delete_offline_package(&package.id, fixture.user_id)
            .await
            .expect("delete original package");
        let replacement = NewOfflinePackage {
            id: package.id.clone(),
            request_id: package.request_id.clone(),
            user_id: package.user_id,
            file_id: package.file_id,
            node_id: package.node_id.clone(),
            source_path: package.source_path.clone(),
            source_size: package.source_size,
            source_mtime: package.source_mtime,
            effective_rate_control: package.effective_rate_control.clone(),
            target_height: package.target_height,
            output_width: package.output_width,
            output_height: package.output_height,
            audio_index: package.audio_index,
            audio_offset_ms: package.audio_offset_ms,
            subtitle_index: package.subtitle_index,
            subtitle_language: package.subtitle_language.clone(),
            subtitle_mode: package.subtitle_mode.clone(),
            estimated_bytes: package.estimated_bytes,
            reserved_bytes: package.reserved_bytes,
            expires_at: i64::MAX,
        };
        fixture
            .store
            .create_offline_package(&replacement, 10, 1_000, 2_000)
            .await
            .expect("replace package");

        let task = tokio::spawn(Arc::clone(&fixture.manager).run());
        for _ in 0..40 {
            tokio::task::yield_now().await;
            if stored_package(&fixture, "changed-source").await.state == "failed" {
                break;
            }
        }
        let failed = stored_package(&fixture, "changed-source").await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.phase, "waiting_for_source");
        assert_eq!(failed.error_code.as_deref(), Some("source_unavailable"));
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_production_returns_to_the_durable_queue() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "yielded", "native", Some(4)).await;
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();

        fixture.manager.prepare(package, &cancelled).await;

        let queued = stored_package(&fixture, "yielded").await;
        assert_eq!(queued.state, "queued");
        assert_eq!(queued.phase, "waiting_for_encoder");
    }

    #[tokio::test]
    async fn invalid_persisted_selection_and_rate_control_fail_before_encoding() {
        let fixture = seeded_fixture().await;
        let invalid_track = claimed_package(&fixture, "invalid-track", "native", None).await;
        fixture
            .manager
            .prepare(invalid_track, &tokio_util::sync::CancellationToken::new())
            .await;
        let invalid_track = stored_package(&fixture, "invalid-track").await;
        assert_eq!(invalid_track.state, "failed");
        assert_eq!(invalid_track.phase, "validating");
        assert_eq!(invalid_track.error_code.as_deref(), Some("invalid_track"));

        let fixture = seeded_fixture().await;
        let mut invalid_rate = claimed_package(&fixture, "invalid-rate", "burned", Some(4)).await;
        invalid_rate.effective_rate_control = "not-a-rate-control".into();
        fixture
            .manager
            .prepare(invalid_rate, &tokio_util::sync::CancellationToken::new())
            .await;
        let invalid_rate = stored_package(&fixture, "invalid-rate").await;
        assert_eq!(invalid_rate.state, "failed");
        assert_eq!(
            invalid_rate.error_code.as_deref(),
            Some("invalid_rate_control")
        );
    }

    #[tokio::test]
    async fn encoder_setup_failure_is_a_typed_terminal_state() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "encoder-failure", "none", None).await;

        fixture
            .manager
            .prepare(package, &tokio_util::sync::CancellationToken::new())
            .await;

        let failed = stored_package(&fixture, "encoder-failure").await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.phase, "transcoding");
        assert_eq!(failed.error_code.as_deref(), Some("encoder_failed"));
    }

    #[tokio::test]
    async fn complete_video_publication_moves_preparing_to_ready_with_verified_state() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "ready-video", "none", None).await;

        fixture
            .manager
            .publish_ready(
                &package,
                &fixture.file,
                produced(90_000, 400),
                Instant::now(),
            )
            .await;

        let ready = stored_package(&fixture, "ready-video").await;
        assert_eq!(ready.state, "ready");
        assert_eq!(ready.phase, "ready");
        assert_eq!(ready.recipe_hash.as_deref(), Some("recipe-under-test"));
        assert_eq!(ready.duration_ms, Some(90_000));
        assert_eq!(
            ready.actual_bytes,
            Some(400 + master_playlist(&package).len() as i64)
        );
        assert!(fixture
            .manager
            .prometheus()
            .contains("plurx_offline_prepare_seconds_count{result=\"ok\"} 1"));
    }

    #[tokio::test]
    async fn incomplete_or_missing_subtitle_publication_never_reports_ready() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "short-video", "none", None).await;
        fixture
            .manager
            .publish_ready(
                &package,
                &fixture.file,
                produced(70_000, 300),
                Instant::now(),
            )
            .await;
        let failed = stored_package(&fixture, "short-video").await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.phase, "publishing");

        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "missing-index", "native", None).await;
        fixture
            .manager
            .publish_ready(
                &package,
                &fixture.file,
                produced(90_000, 300),
                Instant::now(),
            )
            .await;
        let failed = stored_package(&fixture, "missing-index").await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.phase, "extracting_subtitles");
        assert_eq!(failed.error_code.as_deref(), Some("subtitle_failed"));

        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "missing-sidecar", "native", Some(4)).await;
        fixture
            .manager
            .publish_ready(
                &package,
                &fixture.file,
                produced(90_000, 300),
                Instant::now(),
            )
            .await;
        let failed = stored_package(&fixture, "missing-sidecar").await;
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.phase, "extracting_subtitles");
    }

    #[tokio::test]
    async fn native_subtitle_publication_counts_the_complete_rendition() {
        let fixture = seeded_fixture().await;
        let package = claimed_package(&fixture, "native-ready", "native", Some(4)).await;
        let sidecar = crate::subtitles::vtt_path_for_identity(
            fixture.manager.transcode.subtitle_cache_dir(),
            package.file_id,
            4,
            package.source_size,
            package.source_mtime,
        );
        tokio::fs::create_dir_all(sidecar.parent().expect("sidecar parent"))
            .await
            .expect("sidecar parent");
        tokio::fs::write(&sidecar, b"WEBVTT\n\n")
            .await
            .expect("sidecar");

        fixture
            .manager
            .publish_ready(
                &package,
                &fixture.file,
                produced(90_000, 400),
                Instant::now(),
            )
            .await;

        let ready = stored_package(&fixture, "native-ready").await;
        let expected = 400
            + master_playlist(&package).len() as i64
            + b"WEBVTT\n\n".len() as i64
            + subtitle_playlist(90_000).len() as i64;
        assert_eq!(ready.state, "ready");
        assert_eq!(ready.actual_bytes, Some(expected));
    }
}
