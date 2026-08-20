//! Per-user EPUB locator state for the replicated hiqlite backend.

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;

use super::hiqlite::{database_error, validate_sql, HiqliteAuthStore};
use super::ReadingStore;
use crate::domain::{ReadingState, ReadingStateWrite};
use crate::error::StoreError;

struct ReadingRow {
    file_id: i64,
    file_size: i64,
    file_mtime: i64,
    locator_json: String,
    progression_millis: i64,
    completed: i64,
    updated_at: i64,
}

impl From<&mut Row<'_>> for ReadingRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            file_id: row.get("file_id"),
            file_size: row.get("file_size"),
            file_mtime: row.get("file_mtime"),
            locator_json: row.get("locator_json"),
            progression_millis: row.get("progression_millis"),
            completed: row.get("completed"),
            updated_at: row.get("updated_at"),
        }
    }
}

impl From<ReadingRow> for ReadingState {
    fn from(row: ReadingRow) -> Self {
        Self {
            file_id: row.file_id,
            file_size: row.file_size,
            file_mtime: row.file_mtime,
            locator_json: row.locator_json,
            progression_millis: row.progression_millis,
            completed: row.completed != 0,
            updated_at: row.updated_at,
        }
    }
}

#[async_trait]
impl ReadingStore for HiqliteAuthStore {
    async fn reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<Option<ReadingState>, StoreError> {
        let sql = "SELECT file_id, file_size, file_mtime, locator_json, \
                          progression_millis, completed, updated_at \
                   FROM reading_state \
                   WHERE user_id = $1 AND item_id = $2 AND file_id = $3";
        validate_sql(sql)?;
        Ok(self
            .client()
            .query_consistent_map::<ReadingRow, _>(sql, params!(user_id, item_id, file_id))
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into))
    }

    async fn current_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<ReadingState>, StoreError> {
        let sql = "SELECT r.file_id, r.file_size, r.file_mtime, r.locator_json, \
                          r.progression_millis, r.completed, r.updated_at \
                   FROM reading_state r \
                   JOIN files f ON f.id = r.file_id AND f.item_id = r.item_id \
                   WHERE r.user_id = $1 AND r.item_id = $2 \
                     AND r.file_size = f.size AND r.file_mtime = f.mtime \
                   ORDER BY r.updated_at DESC, r.file_id ASC LIMIT 1";
        validate_sql(sql)?;
        Ok(self
            .client()
            .query_consistent_map::<ReadingRow, _>(sql, params!(user_id, item_id))
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into))
    }

    async fn put_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        state: &ReadingStateWrite,
    ) -> Result<ReadingState, StoreError> {
        let now = self.now()?;
        let at = state.recorded_at.unwrap_or(now).clamp(0, now);
        let sql = "INSERT INTO reading_state \
                       (user_id, item_id, file_id, file_size, file_mtime, locator_json, \
                        progression_millis, completed, updated_at) \
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                   ON CONFLICT(user_id, item_id, file_id) DO UPDATE SET \
                       file_size = excluded.file_size, \
                       file_mtime = excluded.file_mtime, \
                       locator_json = excluded.locator_json, \
                       progression_millis = excluded.progression_millis, \
                       completed = excluded.completed, \
                       updated_at = excluded.updated_at \
                   WHERE $10 = 1 \
                      OR excluded.file_size != reading_state.file_size \
                      OR excluded.file_mtime != reading_state.file_mtime \
                      OR excluded.updated_at >= reading_state.updated_at \
                   RETURNING file_id, file_size, file_mtime, locator_json, \
                             progression_millis, completed, updated_at";
        validate_sql(sql)?;
        let returned = self
            .client()
            .execute_returning_map::<_, ReadingRow>(
                sql,
                params!(
                    user_id,
                    item_id,
                    state.file_id,
                    state.file_size,
                    state.file_mtime,
                    &state.locator_json,
                    state.progression_millis,
                    state.completed,
                    at,
                    state.recorded_at.is_none()
                ),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if let Some(row) = returned.into_iter().next() {
            return Ok(row.into());
        }
        self.reading_state(user_id, item_id, state.file_id)
            .await?
            .ok_or_else(|| {
                StoreError::Database("stale reading replay found no current row".to_owned())
            })
    }

    async fn delete_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<(), StoreError> {
        let sql = "DELETE FROM reading_state \
                   WHERE user_id = $1 AND item_id = $2 AND file_id = $3";
        validate_sql(sql)?;
        self.client()
            .execute(sql, params!(user_id, item_id, file_id))
            .await
            .map_err(database_error)?;
        Ok(())
    }
}
