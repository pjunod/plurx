//! Per-user watch state and the continue-watching row.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::{item_cols, item_from_row, SqliteStore};
use crate::domain::{InProgressItem, WatchState};
use crate::error::StoreError;
use crate::store::WatchStore;

/// Fraction of runtime past which an item is considered watched.
const WATCHED_THRESHOLD: f64 = 0.95;

fn watch_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<WatchState> {
    Ok(WatchState {
        position_ms: row.get(base)?,
        duration_ms: row.get(base + 1)?,
        watched: row.get::<_, i64>(base + 2)? != 0,
        updated_at: row.get(base + 3)?,
    })
}

#[async_trait]
impl WatchStore for SqliteStore {
    async fn watch_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<WatchState>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT position_ms, duration_ms, watched, updated_at
                     FROM watch_state WHERE user_id = ?1 AND item_id = ?2",
                    params![user_id, item_id],
                    |row| watch_from_row(row, 0),
                )
                .optional()?)
        })
        .await
    }

    async fn watch_map(
        &self,
        user_id: i64,
        item_ids: &[i64],
    ) -> Result<Vec<(i64, WatchState)>, StoreError> {
        if item_ids.is_empty() {
            return Ok(Vec::new());
        }
        let item_ids = item_ids.to_vec();
        self.with_conn(move |conn| {
            // rarray would need a feature; a temp-free IN via json_each keeps
            // the query parameter-count bounded regardless of list length.
            let ids_json = serde_json::to_string(&item_ids)
                .map_err(|e| StoreError::Database(e.to_string()))?;
            let mut stmt = conn.prepare(
                "SELECT w.item_id, w.position_ms, w.duration_ms, w.watched, w.updated_at
                 FROM watch_state w
                 JOIN json_each(?2) j ON j.value = w.item_id
                 WHERE w.user_id = ?1",
            )?;
            let rows = stmt
                .query_map(params![user_id, ids_json], |row| {
                    Ok((row.get::<_, i64>(0)?, watch_from_row(row, 1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn put_progress(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<WatchState, StoreError> {
        self.with_conn(move |conn| {
            // Auto-mark watched past the threshold; never un-watch here.
            let watched = match duration_ms {
                Some(d) if d > 0 => (position_ms as f64 / d as f64) >= WATCHED_THRESHOLD,
                _ => false,
            };
            let state = conn.query_row(
                "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, watched, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
                 ON CONFLICT(user_id, item_id) DO UPDATE SET
                     position_ms = excluded.position_ms,
                     duration_ms = COALESCE(excluded.duration_ms, watch_state.duration_ms),
                     watched = watch_state.watched OR excluded.watched,
                     updated_at = unixepoch()
                 RETURNING position_ms, duration_ms, watched, updated_at",
                params![user_id, item_id, position_ms, duration_ms, watched as i64],
                |row| watch_from_row(row, 0),
            )?;
            Ok(state)
        })
        .await
    }

    async fn set_watched(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            if watched {
                // Marking watched jumps the position to the end if known.
                conn.execute(
                    "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at)
                     VALUES (?1, ?2, 0, 1, unixepoch())
                     ON CONFLICT(user_id, item_id) DO UPDATE SET
                         watched = 1, updated_at = unixepoch()",
                    params![user_id, item_id],
                )?;
            } else {
                // Un-watching clears progress so it leaves continue-watching.
                conn.execute(
                    "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at)
                     VALUES (?1, ?2, 0, 0, unixepoch())
                     ON CONFLICT(user_id, item_id) DO UPDATE SET
                         watched = 0, position_ms = 0, updated_at = unixepoch()",
                    params![user_id, item_id],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn continue_watching(
        &self,
        user_id: i64,
        limit: i64,
    ) -> Result<Vec<InProgressItem>, StoreError> {
        self.with_conn(move |conn| {
            // In-progress = has a position, not finished. Episodes carry their
            // show's title so a card can read "Severance · S1E3".
            let mut stmt = conn.prepare(&format!(
                "SELECT {i}, show.title,
                        w.position_ms, w.duration_ms, w.watched, w.updated_at
                 FROM watch_state w
                 JOIN items i ON i.id = w.item_id
                 LEFT JOIN items season
                        ON season.id = i.parent_id AND i.kind = 'episode'
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE w.user_id = ?1 AND w.watched = 0 AND w.position_ms > 0
                   AND i.kind IN ('movie','episode')
                 ORDER BY w.updated_at DESC LIMIT ?2",
                i = item_cols("i")
            ))?;
            let rows = stmt
                .query_map(params![user_id, limit], |row| {
                    Ok(InProgressItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(18)?,
                        state: watch_from_row(row, 19)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};
    use crate::store::{LibraryStore, MediaStore, SqliteStore, UserStore, WatchStore};

    #[tokio::test]
    async fn progress_marks_watched_and_drives_continue_row() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");
        let lib = store
            .create_library(&NewLibrary {
                name: "M".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
            })
            .await
            .expect("lib");
        let movie = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Dune".into(),
                year: Some(2021),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");

        // Halfway through → shows up in continue-watching, not watched.
        let state = store
            .put_progress(user.id, movie, 30_000, Some(120_000))
            .await
            .expect("progress");
        assert!(!state.watched);
        let cw = store.continue_watching(user.id, 10).await.expect("cw");
        assert_eq!(cw.len(), 1);
        assert_eq!(cw[0].item.id, movie);
        assert_eq!(cw[0].state.position_ms, 30_000);

        // Batch watch-map returns the same state.
        let map = store.watch_map(user.id, &[movie]).await.expect("map");
        assert_eq!(map.len(), 1);
        assert_eq!(map[0].0, movie);

        // Past 95% → auto-watched, leaves continue-watching.
        let state = store
            .put_progress(user.id, movie, 119_000, Some(120_000))
            .await
            .expect("progress");
        assert!(state.watched);
        assert!(store
            .continue_watching(user.id, 10)
            .await
            .expect("cw")
            .is_empty());

        // Manual un-watch clears it entirely.
        store
            .set_watched(user.id, movie, false)
            .await
            .expect("unwatch");
        let ws = store
            .watch_state(user.id, movie)
            .await
            .expect("ws")
            .expect("present");
        assert!(!ws.watched);
        assert_eq!(ws.position_ms, 0);
    }
}
