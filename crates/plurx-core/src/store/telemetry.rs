//! Node-local playback telemetry storage shared by both durable backends.
//!
//! Single-node SQLite carries this schema as migration v17. A hiqlite voter
//! owns the same table in a separate SQLite sidecar because submitting an
//! operational event through hiqlite's write API would replicate it via Raft.

#[cfg(any(test, feature = "hiqlite-store"))]
use std::path::Path;
#[cfg(any(test, feature = "hiqlite-store"))]
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use crate::domain::{PlaybackEvent, PlaybackEventQuery};
use crate::error::StoreError;

pub(crate) const PLAYBACK_EVENTS_SCHEMA: &str = "
CREATE TABLE playback_events (
    id             INTEGER PRIMARY KEY,
    at_unix_ms     INTEGER NOT NULL,
    user_id        INTEGER,
    session_id     TEXT,
    file_id        INTEGER,
    event          TEXT NOT NULL,
    level          TEXT,
    method         TEXT,
    encoder        TEXT,
    height         INTEGER,
    ms             INTEGER,
    runway_ds      INTEGER,
    bandwidth_kbps INTEGER,
    speed_recent   REAL,
    ahead_seconds  INTEGER,
    suspended      INTEGER,
    hold_reason    TEXT,
    delivered_bps  INTEGER,
    readrate        REAL,
    detail         TEXT,
    attempt        TEXT,
    reason         TEXT,
    ua             TEXT,
    extra          TEXT
) STRICT;
CREATE INDEX playback_events_by_event ON playback_events(event, at_unix_ms);
CREATE INDEX playback_events_by_file ON playback_events(file_id, at_unix_ms);";

#[cfg(any(test, feature = "hiqlite-store"))]
const SIDECAR_SCHEMA_VERSION: i64 = 1;
const MAX_QUERY_ROWS: i64 = 2_000;
const MAX_PRUNE_ROWS: i64 = 10_000;

pub(crate) fn insert(conn: &Connection, event: &PlaybackEvent) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO playback_events (
             at_unix_ms, user_id, session_id, file_id, event, level, method,
             encoder, height, ms, runway_ds, bandwidth_kbps, speed_recent,
             ahead_seconds, suspended, hold_reason, delivered_bps, readrate,
             detail, attempt, reason, ua, extra
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
         )",
        params![
            event.at_unix_ms,
            event.user_id,
            event.session_id,
            event.file_id,
            event.event,
            event.level,
            event.method,
            event.encoder,
            event.height,
            event.ms,
            event.runway_ds,
            event.bandwidth_kbps,
            event.speed_recent,
            event.ahead_seconds,
            event.suspended.map(i64::from),
            event.hold_reason,
            event.delivered_bps,
            event.readrate,
            event.detail,
            event.attempt,
            event.reason,
            event.ua,
            event.extra,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn prune(conn: &Connection, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
    if limit <= 0 {
        return Ok(0);
    }
    let limit = limit.min(MAX_PRUNE_ROWS);
    let changed = conn.execute(
        "DELETE FROM playback_events WHERE id IN (
             SELECT id FROM playback_events
             WHERE at_unix_ms < ?1
             ORDER BY at_unix_ms, id
             LIMIT ?2
         )",
        params![before_ms, limit],
    )?;
    u64::try_from(changed).map_err(|error| StoreError::Task(error.to_string()))
}

pub(crate) fn query(
    conn: &Connection,
    query: &PlaybackEventQuery,
) -> Result<Vec<PlaybackEvent>, StoreError> {
    let limit = query.limit.clamp(1, MAX_QUERY_ROWS);
    let mut statement = conn.prepare(
        "SELECT id, at_unix_ms, user_id, session_id, file_id, event, level,
                method, encoder, height, ms, runway_ds, bandwidth_kbps,
                speed_recent, ahead_seconds, suspended, hold_reason,
                delivered_bps, readrate, detail, attempt, reason, ua, extra
         FROM playback_events
         WHERE (?1 IS NULL OR at_unix_ms >= ?1)
           AND (?2 IS NULL OR event = ?2)
         ORDER BY at_unix_ms DESC, id DESC
         LIMIT ?3",
    )?;
    let rows = statement.query_map(params![query.since_ms, query.event, limit], |row| {
        Ok(PlaybackEvent {
            id: row.get(0)?,
            at_unix_ms: row.get(1)?,
            user_id: row.get(2)?,
            session_id: row.get(3)?,
            file_id: row.get(4)?,
            event: row.get(5)?,
            level: row.get(6)?,
            method: row.get(7)?,
            encoder: row.get(8)?,
            height: row.get(9)?,
            ms: row.get(10)?,
            runway_ds: row.get(11)?,
            bandwidth_kbps: row.get(12)?,
            speed_recent: row.get(13)?,
            ahead_seconds: row.get(14)?,
            suspended: row.get::<_, Option<i64>>(15)?.map(|value| value != 0),
            hold_reason: row.get(16)?,
            delivered_bps: row.get(17)?,
            readrate: row.get(18)?,
            detail: row.get(19)?,
            attempt: row.get(20)?,
            reason: row.get(21)?,
            ua: row.get(22)?,
            extra: row.get(23)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One hiqlite voter's explicitly node-local telemetry database.
#[derive(Clone)]
#[cfg(any(test, feature = "hiqlite-store"))]
pub(crate) struct NodeLocalTelemetry {
    conn: Arc<Mutex<Connection>>,
}

#[cfg(any(test, feature = "hiqlite-store"))]
impl NodeLocalTelemetry {
    pub(crate) fn open(path: &Path) -> Result<Self, StoreError> {
        Self::initialize(Connection::open(path)?)
    }

    fn initialize(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > SIDECAR_SCHEMA_VERSION {
            return Err(StoreError::Migration(format!(
                "telemetry sidecar schema is v{current}, but this binary only knows \
                 v{SIDECAR_SCHEMA_VERSION}"
            )));
        }
        if current == 0 {
            conn.execute_batch(&format!("BEGIN;\n{PLAYBACK_EVENTS_SCHEMA}\nCOMMIT;"))?;
            conn.pragma_update(None, "user_version", SIDECAR_SCHEMA_VERSION)?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| StoreError::Task("telemetry sidecar mutex poisoned".to_owned()))?;
            operation(&guard)
        })
        .await
        .map_err(|error| StoreError::Task(error.to_string()))?
    }

    pub(crate) async fn record(&self, event: PlaybackEvent) -> Result<i64, StoreError> {
        self.with_conn(move |conn| insert(conn, &event)).await
    }

    pub(crate) async fn prune(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| prune(conn, before_ms, limit))
            .await
    }

    pub(crate) async fn events(
        &self,
        query_value: PlaybackEventQuery,
    ) -> Result<Vec<PlaybackEvent>, StoreError> {
        self.with_conn(move |conn| query(conn, &query_value)).await
    }

    #[cfg(feature = "hiqlite-store")]
    pub(crate) async fn clear(&self) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM playback_events", [])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, at_unix_ms: i64) -> PlaybackEvent {
        PlaybackEvent {
            at_unix_ms,
            event: name.to_owned(),
            ..PlaybackEvent::default()
        }
    }

    #[tokio::test]
    async fn sidecar_survives_reopen_and_does_not_share_rows() {
        let root = tempfile::tempdir().expect("sidecar root");
        let first_path = root.path().join("node-1.db");
        let second_path = root.path().join("node-2.db");
        let first = NodeLocalTelemetry::open(&first_path).expect("first sidecar");
        first.record(event("ttff", 10)).await.expect("record");
        drop(first);

        let reopened = NodeLocalTelemetry::open(&first_path).expect("reopen first");
        assert_eq!(
            reopened
                .events(PlaybackEventQuery {
                    limit: 10,
                    ..PlaybackEventQuery::default()
                })
                .await
                .expect("read reopened")
                .len(),
            1
        );
        let second = NodeLocalTelemetry::open(&second_path).expect("second sidecar");
        assert!(second
            .events(PlaybackEventQuery {
                limit: 10,
                ..PlaybackEventQuery::default()
            })
            .await
            .expect("read second")
            .is_empty());
    }

    #[test]
    fn sidecar_refuses_a_future_schema_version() {
        let directory = tempfile::tempdir().expect("sidecar directory");
        let path = directory.path().join("telemetry.db");
        let conn = Connection::open(&path).expect("seed sidecar");
        conn.pragma_update(None, "user_version", SIDECAR_SCHEMA_VERSION + 1)
            .expect("future version");
        drop(conn);
        let error = NodeLocalTelemetry::open(&path)
            .err()
            .expect("future sidecar schema must be refused");
        assert!(error.to_string().contains("only knows v1"), "{error}");
    }
}
