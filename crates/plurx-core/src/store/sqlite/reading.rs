//! Per-user EPUB locator state for the single-node SQLite backend.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::SqliteStore;
use crate::domain::{ReadingState, ReadingStateWrite};
use crate::error::StoreError;
use crate::store::ReadingStore;

fn reading_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<ReadingState> {
    Ok(ReadingState {
        file_id: row.get(base)?,
        file_size: row.get(base + 1)?,
        file_mtime: row.get(base + 2)?,
        locator_json: row.get(base + 3)?,
        progression_millis: row.get(base + 4)?,
        completed: row.get::<_, i64>(base + 5)? != 0,
        updated_at: row.get(base + 6)?,
    })
}

const READING_COLUMNS: &str =
    "file_id, file_size, file_mtime, locator_json, progression_millis, completed, updated_at";
const QUALIFIED_READING_COLUMNS: &str = "r.file_id, r.file_size, r.file_mtime, r.locator_json, \
     r.progression_millis, r.completed, r.updated_at";

#[async_trait]
impl ReadingStore for SqliteStore {
    async fn reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<Option<ReadingState>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {READING_COLUMNS} FROM reading_state \
                         WHERE user_id = ?1 AND item_id = ?2 AND file_id = ?3"
                    ),
                    params![user_id, item_id, file_id],
                    |row| reading_from_row(row, 0),
                )
                .optional()?)
        })
        .await
    }

    async fn current_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<ReadingState>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {QUALIFIED_READING_COLUMNS} FROM reading_state r \
                         JOIN files f ON f.id = r.file_id AND f.item_id = r.item_id \
                         WHERE r.user_id = ?1 AND r.item_id = ?2 \
                           AND r.file_size = f.size AND r.file_mtime = f.mtime \
                         ORDER BY r.updated_at DESC, r.file_id ASC LIMIT 1"
                    ),
                    params![user_id, item_id],
                    |row| reading_from_row(row, 0),
                )
                .optional()?)
        })
        .await
    }

    async fn put_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        state: &ReadingStateWrite,
    ) -> Result<ReadingState, StoreError> {
        let state = state.clone();
        self.with_conn(move |conn| {
            let now = conn.query_row("SELECT unixepoch()", [], |row| row.get::<_, i64>(0))?;
            let at = state.recorded_at.unwrap_or(now).clamp(0, now);
            let returned = conn
                .query_row(
                    "INSERT INTO reading_state
                         (user_id, item_id, file_id, file_size, file_mtime, locator_json,
                          progression_millis, completed, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(user_id, item_id, file_id) DO UPDATE SET
                         file_size = excluded.file_size,
                         file_mtime = excluded.file_mtime,
                         locator_json = excluded.locator_json,
                         progression_millis = excluded.progression_millis,
                         completed = excluded.completed,
                         updated_at = excluded.updated_at
                     WHERE ?10 = 1
                        OR excluded.file_size != reading_state.file_size
                        OR excluded.file_mtime != reading_state.file_mtime
                        OR excluded.updated_at >= reading_state.updated_at
                     RETURNING file_id, file_size, file_mtime, locator_json,
                               progression_millis, completed, updated_at",
                    params![
                        user_id,
                        item_id,
                        state.file_id,
                        state.file_size,
                        state.file_mtime,
                        state.locator_json,
                        state.progression_millis,
                        state.completed,
                        at,
                        state.recorded_at.is_none() as i64,
                    ],
                    |row| reading_from_row(row, 0),
                )
                .optional()?;
            if let Some(row) = returned {
                return Ok(row);
            }
            conn.query_row(
                &format!(
                    "SELECT {READING_COLUMNS} FROM reading_state \
                     WHERE user_id = ?1 AND item_id = ?2 AND file_id = ?3"
                ),
                params![user_id, item_id, state.file_id],
                |row| reading_from_row(row, 0),
            )
            .map_err(Into::into)
        })
        .await
    }

    async fn delete_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM reading_state \
                 WHERE user_id = ?1 AND item_id = ?2 AND file_id = ?3",
                params![user_id, item_id, file_id],
            )?;
            Ok(())
        })
        .await
    }
}
