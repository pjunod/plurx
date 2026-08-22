//! In-memory ring buffer of recent log events, exposed to the admin UI.
//!
//! `docker logs` requires shell access to the host; the settings page should
//! answer "what is the server doing / what just went wrong" directly. The
//! buffer layer sits under the same global `EnvFilter` as the console logger,
//! so it captures exactly what the console captures — bounded, oldest-out.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// The two operator-facing log surfaces. Cluster chatter has its own bounded
/// ring so replication detail cannot evict playback and library diagnostics.
#[derive(Clone)]
pub struct LogBuffers {
    pub general: Arc<LogBuffer>,
    pub cluster: Arc<LogBuffer>,
}

impl Default for LogBuffers {
    fn default() -> Self {
        Self {
            general: Arc::new(LogBuffer::default()),
            cluster: Arc::new(LogBuffer::default()),
        }
    }
}

/// Targets emitted by plurx membership plus its Raft/replication dependencies.
/// Keeping this centralized makes the console and both UI rings agree about
/// which surface owns an event.
pub fn is_cluster_target(target: &str) -> bool {
    [
        "plurx::cluster",
        "plurx_core::cluster",
        "hiqlite",
        "openraft",
    ]
    .iter()
    .any(|prefix| {
        target == *prefix
            || target
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with("::"))
    })
}

/// One captured log event.
#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    /// Unix milliseconds.
    pub ts_ms: i64,
    /// "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE".
    pub level: String,
    /// Module path that emitted the event.
    pub target: String,
    pub message: String,
}

/// Severity rank: lower is more severe (ERROR=0 … TRACE=4).
fn rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 0,
        "WARN" => 1,
        "INFO" => 2,
        "DEBUG" => 3,
        _ => 4,
    }
}

pub struct LogBuffer {
    inner: Mutex<VecDeque<LogEntry>>,
    cap: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        LogBuffer {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    pub fn push(&self, entry: LogEntry) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == self.cap {
            q.pop_front();
        }
        q.push_back(entry);
    }

    /// The most recent `limit` entries at or above `min_level` severity,
    /// returned oldest-first (ready to render top-to-bottom).
    pub fn tail(&self, min_level: &str, limit: usize) -> Vec<LogEntry> {
        let cutoff = rank(min_level);
        let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<LogEntry> = q
            .iter()
            .rev()
            .filter(|e| rank(&e.level) <= cutoff)
            .take(limit)
            .cloned()
            .collect();
        out.reverse();
        out
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        LogBuffer::new(2000)
    }
}

/// `tracing` layer that feeds the buffer.
pub struct BufferLayer(pub Arc<LogBuffer>);

struct MessageVisitor {
    message: String,
    rest: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, " {}={:?}", field.name(), value);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl<S: tracing::Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MessageVisitor {
            message: String::new(),
            rest: String::new(),
        };
        event.record(&mut v);
        let message = if v.message.is_empty() {
            v.rest.trim_start().to_owned()
        } else {
            format!("{}{}", v.message, v.rest)
        };
        let meta = event.metadata();
        self.0.push(LogEntry {
            ts_ms: now_ms(),
            level: meta.level().to_string(),
            target: meta.target().to_owned(),
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, message: &str) -> LogEntry {
        LogEntry {
            ts_ms: 0,
            level: level.to_owned(),
            target: "test".to_owned(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn filters_by_severity_and_caps() {
        let buf = LogBuffer::new(3);
        buf.push(entry("INFO", "one"));
        buf.push(entry("ERROR", "two"));
        buf.push(entry("DEBUG", "three"));

        // "warn" cutoff keeps only the error.
        let warn_up = buf.tail("warn", 10);
        assert_eq!(warn_up.len(), 1);
        assert_eq!(warn_up[0].message, "two");

        // "trace" keeps everything, oldest first.
        let all = buf.tail("trace", 10);
        assert_eq!(
            all.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );

        // Ring: a fourth entry evicts the oldest.
        buf.push(entry("INFO", "four"));
        let all = buf.tail("trace", 10);
        assert_eq!(all.first().expect("entries").message, "two");
        assert_eq!(all.len(), 3);

        // Limit takes the most recent N, still oldest-first.
        let last_two = buf.tail("trace", 2);
        assert_eq!(
            last_two
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>(),
            vec!["three", "four"]
        );
    }

    #[test]
    fn layer_captures_events_with_fields() {
        use tracing_subscriber::prelude::*;
        let buf = Arc::new(LogBuffer::new(10));
        let subscriber = tracing_subscriber::registry().with(BufferLayer(Arc::clone(&buf)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(code = 42, "boom happened");
            tracing::info!("just fyi");
        });
        let all = buf.tail("trace", 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].level, "ERROR");
        // Both the message and the structured field are captured.
        assert!(
            all[0].message.contains("boom happened"),
            "{}",
            all[0].message
        );
        assert!(all[0].message.contains("code=42"), "{}", all[0].message);
        assert!(all[0].ts_ms >= 0);
        // The default buffer is a large, empty ring.
        assert!(LogBuffer::default().tail("trace", 1).is_empty());
    }

    #[test]
    fn cluster_targets_cover_membership_and_raft_without_claiming_other_logs() {
        assert!(is_cluster_target("plurx::cluster"));
        assert!(is_cluster_target("plurx_core::cluster::membership"));
        assert!(is_cluster_target("hiqlite::store"));
        assert!(is_cluster_target("openraft::core"));
        assert!(!is_cluster_target("plurxd::transcode"));
        assert!(!is_cluster_target("plurx_core::store"));
    }

    #[test]
    fn scoped_layers_keep_cluster_events_out_of_the_general_ring() {
        use tracing_subscriber::filter::filter_fn;
        use tracing_subscriber::prelude::*;

        let logs = LogBuffers {
            general: Arc::new(LogBuffer::new(10)),
            cluster: Arc::new(LogBuffer::new(10)),
        };
        let subscriber = tracing_subscriber::registry()
            .with(
                BufferLayer(Arc::clone(&logs.general))
                    .with_filter(filter_fn(|meta| !is_cluster_target(meta.target()))),
            )
            .with(
                BufferLayer(Arc::clone(&logs.cluster))
                    .with_filter(filter_fn(|meta| is_cluster_target(meta.target()))),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "plurxd::transcode", "ordinary");
            tracing::info!(target: "plurx_core::cluster::membership", "membership");
            tracing::debug!(target: "openraft::core", "raft detail");
        });

        assert_eq!(logs.general.tail("trace", 10)[0].message, "ordinary");
        let cluster = logs.cluster.tail("trace", 10);
        assert_eq!(cluster.len(), 2);
        assert_eq!(cluster[0].message, "membership");
        assert_eq!(cluster[1].message, "raft detail");
    }
}
