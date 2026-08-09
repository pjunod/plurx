//! Node-local playback observations and their bounded Prometheus projection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use plurx_core::domain::PlaybackEvent;
use plurx_core::store::{keys, Store};

const TTFF_BUCKETS: [i64; 8] = [100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000];
const METHODS: [&str; 4] = ["direct_play", "remux", "transcode", "unknown"];
const STALL_KINDS: [&str; 4] = ["supply", "decode", "network", "other"];
const HOLD_REASONS: [&str; 4] = ["time", "bytes", "global", "unknown"];
const CACHE_RESULTS: [&str; 3] = ["hit", "miss", "prefix_later"];
const ENCODERS: [&str; 7] = [
    "qsv",
    "nvenc",
    "vaapi",
    "videotoolbox",
    "software",
    "copy",
    "cached",
];

struct PlaybackMetrics {
    ttff_buckets: [[AtomicU64; TTFF_BUCKETS.len() + 1]; METHODS.len()],
    ttff_count: [AtomicU64; METHODS.len()],
    ttff_sum: [AtomicU64; METHODS.len()],
    stalls: [AtomicU64; STALL_KINDS.len()],
    suspends: [AtomicU64; HOLD_REASONS.len()],
    suspended_ms: AtomicU64,
    cache_serves: [AtomicU64; CACHE_RESULTS.len()],
    sessions: [AtomicU64; ENCODERS.len() + 1],
}

impl PlaybackMetrics {
    const fn new() -> Self {
        Self {
            ttff_buckets: [const { [const { AtomicU64::new(0) }; TTFF_BUCKETS.len() + 1] };
                METHODS.len()],
            ttff_count: [const { AtomicU64::new(0) }; METHODS.len()],
            ttff_sum: [const { AtomicU64::new(0) }; METHODS.len()],
            stalls: [const { AtomicU64::new(0) }; STALL_KINDS.len()],
            suspends: [const { AtomicU64::new(0) }; HOLD_REASONS.len()],
            suspended_ms: AtomicU64::new(0),
            cache_serves: [const { AtomicU64::new(0) }; CACHE_RESULTS.len()],
            sessions: [const { AtomicU64::new(0) }; ENCODERS.len() + 1],
        }
    }

    fn record(&self, event: &PlaybackEvent) {
        match event.event.as_str() {
            "ttff" => {
                let Some(ms) = event.ms.filter(|value| *value >= 0) else {
                    return;
                };
                let method = label_index(event.method.as_deref(), &METHODS);
                let bucket = TTFF_BUCKETS
                    .iter()
                    .position(|upper| ms <= *upper)
                    .unwrap_or(TTFF_BUCKETS.len());
                self.ttff_buckets[method][bucket].fetch_add(1, Ordering::Relaxed);
                self.ttff_count[method].fetch_add(1, Ordering::Relaxed);
                self.ttff_sum[method].fetch_add(ms as u64, Ordering::Relaxed);
            }
            "stall" => {
                let detail = event.detail.as_deref().unwrap_or_default();
                let kind = if detail.contains("supply") {
                    "supply"
                } else if detail.contains("decode") || detail.contains("frame") {
                    "decode"
                } else if detail.contains("network") || detail.contains("blocked") {
                    "network"
                } else {
                    "other"
                };
                self.stalls[label_index(Some(kind), &STALL_KINDS)].fetch_add(1, Ordering::Relaxed);
            }
            "suspend" => {
                self.suspends[label_index(event.hold_reason.as_deref(), &HOLD_REASONS)]
                    .fetch_add(1, Ordering::Relaxed);
            }
            "resume" => {
                if let Some(ms) = event.ms.filter(|value| *value > 0) {
                    self.suspended_ms.fetch_add(ms as u64, Ordering::Relaxed);
                }
            }
            "session_start" => {
                let encoder = label_index_with_unknown(event.encoder.as_deref(), &ENCODERS);
                self.sessions[encoder].fetch_add(1, Ordering::Relaxed);
                if let Some(result) = event
                    .extra
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .and_then(|value| {
                        value
                            .get("cache")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    })
                {
                    if let Some(index) = CACHE_RESULTS.iter().position(|label| *label == result) {
                        self.cache_serves[index].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            _ => {}
        }
    }

    fn render(&self) -> String {
        let mut out = String::from(
            "# HELP plurx_ttff_ms Time from play request to first frame.\n\
             # TYPE plurx_ttff_ms histogram\n",
        );
        for (method_index, method) in METHODS.iter().enumerate() {
            let mut cumulative = 0;
            for (bucket_index, upper) in TTFF_BUCKETS.iter().enumerate() {
                cumulative += self.ttff_buckets[method_index][bucket_index].load(Ordering::Relaxed);
                out.push_str(&format!(
                    "plurx_ttff_ms_bucket{{method=\"{method}\",le=\"{upper}\"}} {cumulative}\n"
                ));
            }
            cumulative +=
                self.ttff_buckets[method_index][TTFF_BUCKETS.len()].load(Ordering::Relaxed);
            out.push_str(&format!(
                "plurx_ttff_ms_bucket{{method=\"{method}\",le=\"+Inf\"}} {cumulative}\n\
                 plurx_ttff_ms_sum{{method=\"{method}\"}} {}\n\
                 plurx_ttff_ms_count{{method=\"{method}\"}} {}\n",
                self.ttff_sum[method_index].load(Ordering::Relaxed),
                self.ttff_count[method_index].load(Ordering::Relaxed)
            ));
        }
        render_counters(
            &mut out,
            "plurx_stalls_total",
            "Playback stalls reported by clients.",
            "kind",
            &STALL_KINDS,
            &self.stalls,
        );
        render_counters(
            &mut out,
            "plurx_suspends_total",
            "Encoder ahead-window suspensions.",
            "reason",
            &HOLD_REASONS,
            &self.suspends,
        );
        out.push_str(&format!(
            "# HELP plurx_suspended_seconds_total Seconds encoders spent suspended.\n\
             # TYPE plurx_suspended_seconds_total counter\n\
             plurx_suspended_seconds_total {:.3}\n",
            self.suspended_ms.load(Ordering::Relaxed) as f64 / 1_000.0
        ));
        render_counters(
            &mut out,
            "plurx_cache_serves_total",
            "Playback session starts by transcode-cache result.",
            "result",
            &CACHE_RESULTS,
            &self.cache_serves,
        );
        let session_labels = ENCODERS
            .iter()
            .copied()
            .chain(std::iter::once("other"))
            .collect::<Vec<_>>();
        render_counters(
            &mut out,
            "plurx_sessions_total",
            "Playback sessions started by encoder.",
            "encoder",
            &session_labels,
            &self.sessions,
        );
        out
    }
}

fn label_index<const N: usize>(value: Option<&str>, labels: &[&str; N]) -> usize {
    labels
        .iter()
        .position(|label| Some(*label) == value)
        .unwrap_or(N - 1)
}

fn label_index_with_unknown<const N: usize>(value: Option<&str>, labels: &[&str; N]) -> usize {
    labels
        .iter()
        .position(|label| Some(*label) == value)
        .unwrap_or(N)
}

fn render_counters<const N: usize>(
    out: &mut String,
    name: &str,
    help: &str,
    label_name: &str,
    labels: &[&str],
    values: &[AtomicU64; N],
) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
    for (label, value) in labels.iter().zip(values) {
        out.push_str(&format!(
            "{name}{{{label_name}=\"{label}\"}} {}\n",
            value.load(Ordering::Relaxed)
        ));
    }
}

static METRICS: LazyLock<PlaybackMetrics> = LazyLock::new(PlaybackMetrics::new);

/// Record metrics and persist an event without delaying the caller. The
/// retention setting is read inside the task so setting `0` makes this a true
/// no-op while HTTP ingest can still return its existing 204 immediately.
pub fn emit(store: Arc<dyn Store>, event: PlaybackEvent) {
    tokio::spawn(async move {
        let enabled = store
            .get_setting(keys::TELEMETRY_RETAIN_DAYS)
            .await
            .ok()
            .flatten()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(keys::TELEMETRY_RETAIN_DEFAULT_DAYS)
            > 0;
        if !enabled {
            return;
        }
        METRICS.record(&event);
        if let Err(error) = store.record_playback_event(&event).await {
            tracing::warn!(%error, event = %event.event, "recording playback telemetry failed");
        }
    });
}

pub fn prometheus() -> String {
    METRICS.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_metrics_render_bounded_labels_and_counts() {
        let metrics = PlaybackMetrics::new();
        metrics.record(&PlaybackEvent {
            event: "ttff".into(),
            method: Some("remux".into()),
            ms: Some(684),
            ..PlaybackEvent::default()
        });
        metrics.record(&PlaybackEvent {
            event: "stall".into(),
            detail: Some("supply".into()),
            ..PlaybackEvent::default()
        });
        metrics.record(&PlaybackEvent {
            event: "suspend".into(),
            hold_reason: Some("time".into()),
            ..PlaybackEvent::default()
        });
        metrics.record(&PlaybackEvent {
            event: "resume".into(),
            ms: Some(1_500),
            ..PlaybackEvent::default()
        });
        let text = metrics.render();
        assert!(text.contains("plurx_ttff_ms_count{method=\"remux\"} 1"));
        assert!(text.contains("plurx_ttff_ms_bucket{method=\"remux\",le=\"1000\"} 1"));
        assert!(text.contains("plurx_stalls_total{kind=\"supply\"} 1"));
        assert!(text.contains("plurx_suspends_total{reason=\"time\"} 1"));
        assert!(text.contains("plurx_suspended_seconds_total 1.500"));
        assert!(!text.contains("title="));
        assert!(!text.contains("user="));
        assert!(!text.contains("path="));
    }
}
