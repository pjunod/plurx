//! The watched-notification outbox (master plan §11.1).

use async_trait::async_trait;
use rusqlite::params;

use super::SqliteStore;
use crate::error::StoreError;
use crate::store::{OutboxEntry, WatchedOutboxStore};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl WatchedOutboxStore for SqliteStore {
    async fn enqueue_watched(&self, payload: &str) -> Result<i64, StoreError> {
        let payload = payload.to_owned();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO watched_outbox (payload, next_at, created_at, updated_at)
                 VALUES (?1, ?2, ?2, ?2)",
                params![payload, now()],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    async fn due_watched(&self, limit: i64) -> Result<Vec<OutboxEntry>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, payload, attempts, last_error, status, next_at
                   FROM watched_outbox
                  WHERE status = 'pending' AND next_at <= ?1
                  ORDER BY next_at, id
                  LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![now(), limit], |row| {
                    Ok(OutboxEntry {
                        id: row.get(0)?,
                        payload: row.get(1)?,
                        attempts: row.get(2)?,
                        last_error: row.get(3)?,
                        status: row.get(4)?,
                        next_at: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn settle_watched(&self, entry: &OutboxEntry) -> Result<(), StoreError> {
        let entry = entry.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE watched_outbox
                    SET attempts = ?2, last_error = ?3, status = ?4, next_at = ?5,
                        updated_at = ?6
                  WHERE id = ?1",
                params![
                    entry.id,
                    entry.attempts,
                    entry.last_error,
                    entry.status,
                    entry.next_at,
                    now()
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn watched_outbox_counts(&self) -> Result<(i64, i64, i64), StoreError> {
        self.with_conn(move |conn| {
            let one = |status: &str| -> rusqlite::Result<i64> {
                conn.query_row(
                    "SELECT COUNT(*) FROM watched_outbox WHERE status = ?1",
                    params![status],
                    |r| r.get(0),
                )
            };
            Ok((one("pending")?, one("ok")?, one("failed")?))
        })
        .await
    }
}
