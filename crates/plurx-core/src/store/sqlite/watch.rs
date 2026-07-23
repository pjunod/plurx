//! Per-user watch state and the continue-watching row.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::{item_cols, item_from_row, SqliteStore};
use crate::domain::{InProgressItem, RecentItem, WatchState};
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
            // The client's idea of duration is untrustworthy: progressive
            // remux and growing HLS playlists report a duration that climbs
            // as data arrives, so position/duration would cross the watched
            // threshold after five minutes of a two-hour film. The probe
            // duration recorded at scan time is authoritative; the client's
            // number is only a fallback for files ffprobe couldn't time.
            let known: Option<i64> = conn
                .query_row(
                    "SELECT duration_ms FROM files
                     WHERE item_id = ?1 AND duration_ms IS NOT NULL AND duration_ms > 0
                     ORDER BY duration_ms DESC LIMIT 1",
                    params![item_id],
                    |row| row.get(0),
                )
                .optional()?;
            let effective = known.or(duration_ms).filter(|d| *d > 0);

            // Auto-mark watched past the threshold; never un-watch here.
            let watched = match effective {
                Some(d) => (position_ms as f64 / d as f64) >= WATCHED_THRESHOLD,
                None => false,
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
                params![user_id, item_id, position_ms, effective, watched as i64],
                |row| watch_from_row(row, 0),
            )?;
            Ok(state)
        })
        .await
    }

    async fn apply_remote_watch(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
        position_ms: i64,
        duration_ms: Option<i64>,
        updated_at: i64,
    ) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            // The remote timestamp lands verbatim (never in the future of the
            // local clock, so a remote clock skew can't freeze later edits).
            let now = conn.query_row("SELECT unixepoch()", [], |r| r.get::<_, i64>(0))?;
            let at = updated_at.clamp(0, now);
            conn.execute(
                "INSERT INTO watch_state
                   (user_id, item_id, position_ms, duration_ms, watched, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id, item_id) DO UPDATE SET
                     position_ms = excluded.position_ms,
                     duration_ms = COALESCE(excluded.duration_ms, watch_state.duration_ms),
                     watched = excluded.watched,
                     updated_at = excluded.updated_at",
                params![
                    user_id,
                    item_id,
                    position_ms,
                    duration_ms,
                    watched as i64,
                    at
                ],
            )?;
            Ok(())
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

    async fn next_up(&self, user_id: i64, limit: i64) -> Result<Vec<RecentItem>, StoreError> {
        self.with_conn(move |conn| {
            // Episode ordering key = season*100000 + episode. Next-up per show
            // is the smallest-ordering episode that is unwatched and not in
            // progress, strictly after the last watched episode of that show.
            // One row per show (bare columns alongside MIN() pick that row).
            let mut stmt = conn.prepare(&format!(
                "SELECT {e}, show.title, MIN(season.season_number*100000 + e.episode_number) AS ord
                 FROM items e
                 JOIN items season ON season.id = e.parent_id
                 JOIN items show ON show.id = season.parent_id
                 WHERE e.kind = 'episode'
                   AND e.id NOT IN (
                       SELECT item_id FROM watch_state
                       WHERE user_id = ?1 AND (watched = 1 OR position_ms > 0))
                   AND (season.season_number*100000 + e.episode_number) > (
                       SELECT COALESCE(MAX(se.season_number*100000 + ep.episode_number), -1)
                       FROM watch_state w
                       JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode'
                       JOIN items se ON se.id = ep.parent_id
                       WHERE w.user_id = ?1 AND w.watched = 1 AND se.parent_id = show.id)
                   AND show.id IN (
                       SELECT sh.id FROM watch_state w
                       JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode'
                       JOIN items se ON se.id = ep.parent_id
                       JOIN items sh ON sh.id = se.parent_id
                       WHERE w.user_id = ?1 AND w.watched = 1)
                   -- A show with an in-progress episode is shown in
                   -- continue-watching instead, so exclude it here.
                   AND show.id NOT IN (
                       SELECT sh.id FROM watch_state w
                       JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode'
                       JOIN items se ON se.id = ep.parent_id
                       JOIN items sh ON sh.id = se.parent_id
                       WHERE w.user_id = ?1 AND w.watched = 0 AND w.position_ms > 0)
                 GROUP BY show.id
                 ORDER BY show.sort_title
                 LIMIT ?2",
                e = item_cols("e")
            ))?;
            let rows = stmt
                .query_map(params![user_id, limit], |row| {
                    Ok(RecentItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(18)?,
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
                anime: false,
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

    #[tokio::test]
    async fn probe_duration_beats_client_duration() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");
        let lib = store
            .create_library(&NewLibrary {
                name: "M".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        // The scan recorded the real runtime: 100 minutes.
        let probe = crate::domain::ProbeResult {
            duration_ms: Some(6_000_000),
            ..Default::default()
        };
        store
            .upsert_file(movie, "/m/Heat (1995).mkv", 1, 1, &probe)
            .await
            .expect("file");

        // A progressive remux reports a *growing* duration: five minutes in,
        // the client says duration ≈ position. Trusting it would mark the
        // film watched at 5/100 minutes. The probe duration must win.
        let state = store
            .put_progress(user.id, movie, 300_000, Some(301_000))
            .await
            .expect("progress");
        assert!(!state.watched, "5 of 100 minutes is not watched");
        assert_eq!(state.duration_ms, Some(6_000_000), "server duration wins");

        // Real completion still auto-marks watched, client duration or not.
        let state = store
            .put_progress(user.id, movie, 5_800_000, None)
            .await
            .expect("progress");
        assert!(state.watched);
    }

    #[tokio::test]
    async fn next_up_surfaces_the_following_episode() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");
        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
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
        let mut eps = Vec::new();
        for n in 1..=3 {
            eps.push(
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Episode,
                        parent_id: Some(season),
                        title: format!("Episode {n}"),
                        year: None,
                        season_number: Some(1),
                        episode_number: Some(n),
                    })
                    .await
                    .expect("ep"),
            );
        }

        // Nothing watched yet → no next-up.
        assert!(store.next_up(user.id, 10).await.expect("nu").is_empty());

        // Watch E1 → next-up is E2.
        store.set_watched(user.id, eps[0], true).await.expect("w");
        let nu = store.next_up(user.id, 10).await.expect("nu");
        assert_eq!(nu.len(), 1);
        assert_eq!(nu[0].item.id, eps[1]);
        assert_eq!(nu[0].show_title.as_deref(), Some("Severance"));

        // Start E2 (in progress) → it moves to continue-watching, not next-up.
        store
            .put_progress(user.id, eps[1], 5_000, Some(60_000))
            .await
            .expect("prog");
        assert!(store.next_up(user.id, 10).await.expect("nu").is_empty());

        // Finish E2 → next-up becomes E3.
        store.set_watched(user.id, eps[1], true).await.expect("w");
        let nu = store.next_up(user.id, 10).await.expect("nu");
        assert_eq!(nu.len(), 1);
        assert_eq!(nu[0].item.id, eps[2]);
    }
}
