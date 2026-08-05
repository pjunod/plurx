//! Durable offline-package preparation coordinator.
//!
//! JSON handlers only create intent. This loop owns the long-running work so
//! an HTTP disconnect or daemon restart cannot abandon a traveller's queue.

use std::sync::Arc;
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

pub struct OfflineManager {
    store: Arc<dyn Store>,
    transcode: Arc<TranscodeManager>,
    node_id: String,
    active: tokio::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
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
        })
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
                )
                .await;
                return;
            }
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "offline source lookup failed");
                self.requeue(&package).await;
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
                )
                .await;
                return;
            }
        };
        let spec = OfflineSpec {
            target_height: package.target_height,
            audio_index: package.audio_index,
            subtitle,
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
                self.publish_ready(&package, &file, produced).await
            }
            Ok(OfflineProduceOutcome::Yielded) | Ok(OfflineProduceOutcome::ClaimedElsewhere) => {
                self.requeue(&package).await
            }
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "offline preparation failed");
                self.fail(
                    &package,
                    "transcoding",
                    "encoder_failed",
                    "The server could not prepare this download.",
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
                &produced.recipe,
                actual_bytes,
                produced.duration_ms,
            )
            .await
        {
            Ok(true) => tracing::info!(
                package = %package.id,
                recipe = %produced.recipe,
                bytes = actual_bytes,
                duration_ms = produced.duration_ms,
                "offline package ready"
            ),
            Ok(false) => tracing::debug!(package = %package.id, "offline package was cancelled"),
            Err(error) => {
                tracing::warn!(package = %package.id, %error, "could not publish offline state")
            }
        }
    }

    async fn requeue(&self, package: &OfflinePackage) {
        if let Err(error) = self.store.requeue_offline_package(&package.id).await {
            tracing::warn!(package = %package.id, %error, "could not requeue offline package");
        }
        tokio::time::sleep(IDLE_POLL).await;
    }

    async fn fail(&self, package: &OfflinePackage, phase: &str, code: &str, message: &str) {
        if let Err(error) = self
            .store
            .fail_offline_package(&package.id, phase, code, message)
            .await
        {
            tracing::warn!(package = %package.id, %error, "could not record offline failure");
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
    }

    #[test]
    fn subtitle_package_is_one_complete_vod_segment() {
        let playlist = subtitle_playlist(90_500);
        assert!(playlist.contains("#EXT-X-TARGETDURATION:91"));
        assert!(playlist.contains("#EXTINF:90.500000"));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }
}
