//! Per-user watch state and the continue-watching row.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::{item_cols, item_from_row, SqliteStore, ITEM_COL_COUNT};
use crate::domain::{InProgressItem, RecentItem, WatchRollup, WatchState};
use crate::error::StoreError;
use crate::store::WatchStore;

/// Fraction of runtime past which an item is considered watched.
const WATCHED_THRESHOLD: f64 = 0.95;

/// Kinds that carry watch state. Photos are excluded deliberately: a home
/// library full of stills would otherwise make every folder permanently
/// unwatched, since nothing ever marks a picture seen.
const PLAYABLE_KINDS: &str = "'movie','episode','video'";

/// Every playable item at or under `item_id`, depth-first through whatever
/// container chain sits above it — season → episode, show → season → episode,
/// or the arbitrarily deep folder trees a home library mirrors from disk.
/// A movie has no children and returns just itself.
fn playable_leaves(conn: &rusqlite::Connection, item_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "WITH RECURSIVE tree(id) AS (
             SELECT id FROM items WHERE id = ?1
             -- UNION, not UNION ALL: it dedupes, so a corrupt parent cycle
             -- terminates instead of spinning the recursion forever.
             UNION
             SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
         )
         SELECT i.id FROM tree t JOIN items i ON i.id = t.id
         WHERE i.kind IN ({PLAYABLE_KINDS})
         ORDER BY i.id"
    ))?;
    let ids = stmt
        .query_map(params![item_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

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

    async fn put_progress_at(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
        recorded_at: Option<i64>,
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

            // A position past the end is either a client bug or a stream whose
            // own duration outgrew the probe. Storing it verbatim would leave a
            // resume point beyond the last frame, so clamp to the runtime; the
            // item still counts as finished, it just no longer claims to be
            // finished somewhere that doesn't exist.
            let position_ms = match effective {
                Some(d) => position_ms.clamp(0, d),
                None => position_ms.max(0),
            };

            // Auto-mark watched past the threshold; never un-watch here.
            let watched = match effective {
                Some(d) => (position_ms as f64 / d as f64) >= WATCHED_THRESHOLD,
                None => false,
            };
            let now = conn.query_row("SELECT unixepoch()", [], |row| row.get::<_, i64>(0))?;
            let at = recorded_at.unwrap_or(now).clamp(0, now);
            let state = conn.query_row(
                "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, watched, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id, item_id) DO UPDATE SET
                     position_ms = excluded.position_ms,
                     duration_ms = COALESCE(excluded.duration_ms, watch_state.duration_ms),
                     watched = watch_state.watched OR excluded.watched,
                     updated_at = excluded.updated_at
                 -- A dated replay must not rewind a newer device. An ordinary
                 -- online heartbeat is authoritative now even if an imported
                 -- clock previously left a future timestamp in this row.
                 WHERE ?7 = 1 OR excluded.updated_at >= watch_state.updated_at
                 RETURNING position_ms, duration_ms, watched, updated_at",
                params![
                    user_id,
                    item_id,
                    position_ms,
                    effective,
                    watched as i64,
                    at,
                    recorded_at.is_none() as i64
                ],
                |row| watch_from_row(row, 0),
            ).or_else(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    conn.query_row(
                        "SELECT position_ms, duration_ms, watched, updated_at
                         FROM watch_state WHERE user_id = ?1 AND item_id = ?2",
                        params![user_id, item_id],
                        |row| watch_from_row(row, 0),
                    )
                } else {
                    Err(error)
                }
            })?;
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

    async fn set_watched_tree(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<Vec<i64>, StoreError> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let ids = playable_leaves(&tx, item_id)?;
            // The upsert's DO UPDATE carries a WHERE, so a row already in the
            // target state is left alone entirely — `execute` returns 0 and the
            // id never enters `changed`. That is what keeps re-marking a
            // finished series from re-notifying about all forty episodes, and
            // it keeps `updated_at` honest: it means "when this changed", not
            // "when someone last clicked the button".
            let mut changed = Vec::new();
            {
                let mut stmt = if watched {
                    tx.prepare(
                        "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at)
                         VALUES (?1, ?2, 0, 1, unixepoch())
                         ON CONFLICT(user_id, item_id) DO UPDATE SET
                             watched = 1, updated_at = unixepoch()
                         WHERE watch_state.watched = 0",
                    )?
                } else {
                    // Un-watching clears progress too, so a half-watched episode
                    // counts as changed even though its flag was already 0 —
                    // otherwise it would linger in continue-watching.
                    tx.prepare(
                        "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at)
                         VALUES (?1, ?2, 0, 0, unixepoch())
                         ON CONFLICT(user_id, item_id) DO UPDATE SET
                             watched = 0, position_ms = 0, updated_at = unixepoch()
                         WHERE watch_state.watched = 1 OR watch_state.position_ms <> 0",
                    )?
                };
                for id in ids {
                    if stmt.execute(params![user_id, id])? > 0 {
                        changed.push(id);
                    }
                }
            }
            tx.commit()?;
            Ok(changed)
        })
        .await
    }

    async fn watch_rollup(&self, user_id: i64, item_id: i64) -> Result<WatchRollup, StoreError> {
        self.with_conn(move |conn| {
            let (leaves, watched) = conn.query_row(
                &format!(
                    "WITH RECURSIVE tree(id) AS (
                         SELECT id FROM items WHERE id = ?2
                         UNION
                         SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                     )
                     SELECT COUNT(*),
                            COALESCE(SUM(w.watched), 0)
                     FROM tree t
                     JOIN items i ON i.id = t.id
                     LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?1
                     WHERE i.kind IN ({PLAYABLE_KINDS})"
                ),
                params![user_id, item_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?;
            Ok(WatchRollup { leaves, watched })
        })
        .await
    }

    async fn watch_rollups(
        &self,
        user_id: i64,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, WatchRollup>, StoreError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // ids are our own row ids (trusted i64s), so an inline IN-list is
        // safe — same reasoning `child_counts` runs on.
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            // One walk for the whole page: the recursion carries the root it
            // started from alongside each descendant, so a single pass can
            // group the leaf counts back onto the containers that asked.
            // UNION (not UNION ALL) still dedupes, so a parent cycle
            // terminates — and a (root, id) pair is unique per root, so two
            // containers on the same page never contaminate each other's
            // count.
            let mut stmt = conn.prepare(&format!(
                "WITH RECURSIVE tree(root, id) AS (
                     SELECT id, id FROM items WHERE id IN ({list})
                     UNION
                     SELECT t.root, i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 SELECT t.root, COUNT(*), COALESCE(SUM(w.watched), 0)
                 FROM tree t
                 JOIN items i ON i.id = t.id
                 LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?1
                 WHERE i.kind IN ({PLAYABLE_KINDS})
                 GROUP BY t.root"
            ))?;
            let rows = stmt
                .query_map(params![user_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        WatchRollup {
                            leaves: row.get(1)?,
                            watched: row.get(2)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A container with nothing playable under it produces no group,
            // and the single-item version answers 0/0 for exactly that case.
            // Seed the map so both agree.
            let mut out: std::collections::HashMap<i64, WatchRollup> = ids
                .into_iter()
                .map(|id| (id, WatchRollup::default()))
                .collect();
            out.extend(rows);
            Ok(out)
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
                        w.position_ms, w.duration_ms, w.watched, w.updated_at,
                        season.poster_path
                 FROM watch_state w
                 JOIN items i ON i.id = w.item_id
                 LEFT JOIN items season
                        ON season.id = i.parent_id AND i.kind = 'episode'
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE w.user_id = ?1 AND w.watched = 0 AND w.position_ms > 0
                   AND i.kind IN ('movie','episode','video')
                 ORDER BY w.updated_at DESC LIMIT ?2",
                i = item_cols("i")
            ))?;
            let rows = stmt
                .query_map(params![user_id, limit], |row| {
                    Ok(InProgressItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(ITEM_COL_COUNT)?,
                        state: watch_from_row(row, ITEM_COL_COUNT + 1)?,
                        season_poster: row.get(ITEM_COL_COUNT + 5)?,
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
                "SELECT {e}, show.title, season.poster_path,
                        MIN(season.season_number*100000 + e.episode_number) AS ord
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
                        show_title: row.get(ITEM_COL_COUNT)?,
                        season_poster: row.get(ITEM_COL_COUNT + 1)?,
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
    use crate::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, WatchRollup};
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
    async fn position_is_clamped_to_the_runtime() {
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
                title: "Ronin".into(),
                year: Some(1998),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let probe = crate::domain::ProbeResult {
            duration_ms: Some(7_320_000),
            ..Default::default()
        };
        store
            .upsert_file(movie, "/m/Ronin (1998).mkv", 1, 1, &probe)
            .await
            .expect("file");

        // A stream whose container padding runs past the probed runtime would
        // otherwise leave a resume point beyond the last frame.
        let state = store
            .put_progress(user.id, movie, 9_000_000, None)
            .await
            .expect("progress");
        assert_eq!(state.position_ms, 7_320_000, "clamped to the runtime");
        assert!(state.watched, "still counts as finished");

        // And a negative position never reaches the database.
        let state = store
            .put_progress(user.id, movie, -5_000, None)
            .await
            .expect("progress");
        assert_eq!(state.position_ms, 0);
    }

    #[tokio::test]
    async fn timestamped_progress_refuses_stale_offline_replays() {
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
                title: "Arrival".into(),
                year: Some(2016),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");

        let first = store
            .put_progress_at(user.id, movie, 70_000, Some(100_000), Some(200))
            .await
            .expect("newer progress");
        assert_eq!(first.position_ms, 70_000);

        let stale = store
            .put_progress_at(user.id, movie, 20_000, Some(100_000), Some(100))
            .await
            .expect("stale progress returns current state");
        assert_eq!(stale.position_ms, 70_000, "late replay must not rewind");
        assert_eq!(stale.updated_at, 200);

        let finished = store
            .put_progress_at(user.id, movie, 98_000, Some(100_000), Some(300))
            .await
            .expect("newest progress");
        assert!(finished.watched);

        let stale_unfinished = store
            .put_progress_at(user.id, movie, 30_000, Some(100_000), Some(250))
            .await
            .expect("stale unfinished progress");
        assert!(
            stale_unfinished.watched,
            "progress never implicitly un-watches"
        );
        assert_eq!(stale_unfinished.position_ms, 98_000);
    }

    #[tokio::test]
    async fn online_progress_bypasses_a_future_imported_clock() {
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
                title: "Arrival".into(),
                year: Some(2016),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .put_progress(user.id, movie, 10_000, Some(100_000))
            .await
            .expect("initial");
        let future = 4_000_000_000_i64;
        store
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE watch_state SET updated_at = ?3 WHERE user_id = ?1 AND item_id = ?2",
                    rusqlite::params![user.id, movie, future],
                )?;
                Ok(())
            })
            .await
            .expect("inject skew");

        let current = store
            .put_progress(user.id, movie, 40_000, Some(100_000))
            .await
            .expect("online heartbeat");
        assert_eq!(current.position_ms, 40_000);
        assert!(current.updated_at < future);
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

    #[tokio::test]
    async fn marking_a_show_reaches_every_episode_under_it() {
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
        // Two seasons, so the walk has to go show → season → episode rather
        // than picking up direct children only.
        let mut seasons = Vec::new();
        let mut eps = Vec::new();
        for s in 1..=2 {
            let season = store
                .insert_item(&NewItem {
                    library_id: lib.id,
                    kind: ItemKind::Season,
                    parent_id: Some(show),
                    title: format!("Season {s}"),
                    year: None,
                    season_number: Some(s),
                    episode_number: None,
                })
                .await
                .expect("season");
            seasons.push(season);
            for n in 1..=2 {
                eps.push(
                    store
                        .insert_item(&NewItem {
                            library_id: lib.id,
                            kind: ItemKind::Episode,
                            parent_id: Some(season),
                            title: format!("S{s}E{n}"),
                            year: None,
                            season_number: Some(s),
                            episode_number: Some(n),
                        })
                        .await
                        .expect("ep"),
                );
            }
        }

        // Nothing seen yet.
        let r = store.watch_rollup(user.id, show).await.expect("rollup");
        assert_eq!((r.leaves, r.watched), (4, 0));
        assert!(!r.complete());

        // Mark the series watched: all four episodes, and only the episodes —
        // the show and season rows are containers, not things you watch.
        let changed = store
            .set_watched_tree(user.id, show, true)
            .await
            .expect("mark");
        assert_eq!(changed.len(), 4);
        assert_eq!(changed, eps);
        for ep in &eps {
            let w = store.watch_state(user.id, *ep).await.expect("w");
            assert!(w.expect("row").watched, "episode {ep} should be watched");
        }
        assert!(store
            .watch_state(user.id, show)
            .await
            .expect("show")
            .is_none());
        assert!(store
            .watch_rollup(user.id, show)
            .await
            .expect("r")
            .complete());

        // Next Up is the reason this cascades: marking only the show row would
        // leave the badge saying watched while the rail offered episode one.
        assert!(store.next_up(user.id, 10).await.expect("nu").is_empty());

        // Doing it again changes nothing, so nothing is reported — that is what
        // keeps a second click from re-announcing all four episodes.
        assert!(store
            .set_watched_tree(user.id, show, true)
            .await
            .expect("again")
            .is_empty());

        // One season un-watches its own episodes and leaves the other alone.
        let changed = store
            .set_watched_tree(user.id, seasons[0], false)
            .await
            .expect("unmark");
        assert_eq!(changed, eps[..2].to_vec());
        let r = store.watch_rollup(user.id, show).await.expect("rollup");
        assert_eq!((r.leaves, r.watched), (4, 2));
        let r = store.watch_rollup(user.id, seasons[1]).await.expect("s2");
        assert!(r.complete(), "season two is untouched");
    }

    #[tokio::test]
    async fn un_watching_clears_a_half_finished_episode() {
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

        // A movie is its own tree of one, and a rollup answers 0/1 for it.
        let r = store.watch_rollup(user.id, movie).await.expect("rollup");
        assert_eq!((r.leaves, r.watched), (1, 0));

        store
            .put_progress(user.id, movie, 40_000, Some(170_000))
            .await
            .expect("prog");
        assert_eq!(
            store
                .continue_watching(user.id, 10)
                .await
                .expect("cw")
                .len(),
            1
        );

        // The flag was already 0, but the position wasn't — so this is a real
        // change, and it has to be reported as one or the film would sit in
        // continue-watching with the UI insisting nothing happened.
        let changed = store
            .set_watched_tree(user.id, movie, false)
            .await
            .expect("unmark");
        assert_eq!(changed, vec![movie]);
        assert!(store
            .continue_watching(user.id, 10)
            .await
            .expect("cw")
            .is_empty());
    }

    /// The library grid's "Watched"/"In progress" filters ask a question
    /// about containers, and a container has no watch row — so the grid gets
    /// rollups or it gets nothing. Batched, because a page of a show library
    /// is tens of containers and a rollup is a recursive walk.
    #[tokio::test]
    async fn a_page_of_containers_rolls_up_in_one_pass_and_agrees_with_the_single_walk() {
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
        let show = |title: &str| {
            let title = title.to_owned();
            async {
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Show,
                        parent_id: None,
                        title,
                        year: Some(2022),
                        season_number: None,
                        episode_number: None,
                    })
                    .await
                    .expect("show")
            }
        };
        let finished = show("Finished").await;
        let half = show("Half").await;
        let untouched = show("Untouched").await;
        let empty = show("Announced").await;

        let mut episodes = std::collections::HashMap::new();
        for parent in [finished, half, untouched] {
            let season = store
                .insert_item(&NewItem {
                    library_id: lib.id,
                    kind: ItemKind::Season,
                    parent_id: Some(parent),
                    title: "Season 1".into(),
                    year: None,
                    season_number: Some(1),
                    episode_number: None,
                })
                .await
                .expect("season");
            let mut eps = Vec::new();
            for n in 1..=2 {
                eps.push(
                    store
                        .insert_item(&NewItem {
                            library_id: lib.id,
                            kind: ItemKind::Episode,
                            parent_id: Some(season),
                            title: format!("E{n}"),
                            year: None,
                            season_number: Some(1),
                            episode_number: Some(n),
                        })
                        .await
                        .expect("ep"),
                );
            }
            episodes.insert(parent, (season, eps));
        }

        store
            .set_watched_tree(user.id, finished, true)
            .await
            .expect("mark");
        store
            .set_watched(user.id, episodes[&half].1[0], true)
            .await
            .expect("mark");

        let containers = [finished, half, untouched, empty];
        let rollups = store
            .watch_rollups(user.id, &containers)
            .await
            .expect("rollups");
        assert_eq!(rollups.len(), 4, "every id asked about gets an answer");
        assert_eq!(
            rollups[&finished],
            WatchRollup {
                leaves: 2,
                watched: 2
            }
        );
        assert_eq!(
            rollups[&half],
            WatchRollup {
                leaves: 2,
                watched: 1
            }
        );
        assert_eq!(
            rollups[&untouched],
            WatchRollup {
                leaves: 2,
                watched: 0
            }
        );
        assert_eq!(
            rollups[&empty],
            WatchRollup::default(),
            "a show with nothing in it yet is 0/0, not absent"
        );
        assert!(rollups[&finished].complete());
        assert!(!rollups[&empty].complete());

        // The batched walk and the per-item walk are the same question asked
        // two ways; if they can disagree, a grid card and its detail page can
        // disagree. Seasons too, since the grid paints those as cards as well.
        for id in containers
            .iter()
            .copied()
            .chain(episodes.values().map(|(season, _)| *season))
        {
            let one = store.watch_rollup(user.id, id).await.expect("single");
            let batched = store.watch_rollups(user.id, &[id]).await.expect("batched")[&id];
            assert_eq!(one, batched, "rollups disagree for item {id}");
        }

        // Two containers on the same page must not contaminate each other:
        // one of them being fully watched cannot lift the other's count.
        let together = store
            .watch_rollups(user.id, &[finished, untouched])
            .await
            .expect("pair");
        assert_eq!(together[&untouched].watched, 0);

        assert!(store
            .watch_rollups(user.id, &[])
            .await
            .expect("none")
            .is_empty());
    }
}
