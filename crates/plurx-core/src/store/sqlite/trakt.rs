//! Trakt account links and the sync-candidate join.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension, Row};

use super::SqliteStore;
use crate::domain::TraktAuth;
use crate::error::StoreError;
use crate::store::TraktStore;
use crate::trakt::{Ident, LocalWatch, SyncCandidate};

fn auth_from_row(row: &Row<'_>) -> rusqlite::Result<TraktAuth> {
    Ok(TraktAuth {
        user_id: row.get(0)?,
        access_token: row.get(1)?,
        refresh_token: row.get(2)?,
        expires_at: row.get(3)?,
        trakt_username: row.get(4)?,
        connected_at: row.get(5)?,
        last_sync_at: row.get(6)?,
        last_activities: row.get(7)?,
    })
}

const AUTH_COLS: &str = "user_id, access_token, refresh_token, expires_at, trakt_username, \
     connected_at, last_sync_at, last_activities";

#[async_trait]
impl TraktStore for SqliteStore {
    async fn get_trakt_auth(&self, user_id: i64) -> Result<Option<TraktAuth>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {AUTH_COLS} FROM trakt_auth WHERE user_id = ?1"),
                    params![user_id],
                    auth_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn list_trakt_auth(&self) -> Result<Vec<TraktAuth>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {AUTH_COLS} FROM trakt_auth ORDER BY user_id"))?;
            let rows = stmt
                .query_map([], auth_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn put_trakt_auth(&self, auth: &TraktAuth) -> Result<(), StoreError> {
        let auth = auth.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO trakt_auth
                   (user_id, access_token, refresh_token, expires_at, trakt_username,
                    connected_at, last_sync_at, last_activities)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(user_id) DO UPDATE SET
                     access_token = excluded.access_token,
                     refresh_token = excluded.refresh_token,
                     expires_at = excluded.expires_at,
                     trakt_username = excluded.trakt_username,
                     connected_at = excluded.connected_at,
                     last_sync_at = excluded.last_sync_at,
                     last_activities = excluded.last_activities",
                params![
                    auth.user_id,
                    auth.access_token,
                    auth.refresh_token,
                    auth.expires_at,
                    auth.trakt_username,
                    auth.connected_at,
                    auth.last_sync_at,
                    auth.last_activities,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_trakt_auth(&self, user_id: i64) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM trakt_auth WHERE user_id = ?1", params![user_id])?;
            Ok(())
        })
        .await
    }

    async fn update_trakt_tokens(
        &self,
        user_id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        let (access_token, refresh_token) = (access_token.to_owned(), refresh_token.to_owned());
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE trakt_auth SET access_token = ?2, refresh_token = ?3, expires_at = ?4
                 WHERE user_id = ?1",
                params![user_id, access_token, refresh_token, expires_at],
            )?;
            Ok(())
        })
        .await
    }

    async fn set_trakt_sync(
        &self,
        user_id: i64,
        last_sync_at: i64,
        last_activities: Option<&str>,
    ) -> Result<(), StoreError> {
        let last_activities = last_activities.map(str::to_owned);
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE trakt_auth SET last_sync_at = ?2, last_activities = ?3
                 WHERE user_id = ?1",
                params![user_id, last_sync_at, last_activities],
            )?;
            Ok(())
        })
        .await
    }

    async fn trakt_sync_candidates(
        &self,
        user_id: i64,
    ) -> Result<Vec<SyncCandidate>, StoreError> {
        self.with_conn(move |conn| {
            // Movies key on their own TMDB id; episodes on the show's TMDB id
            // plus season/episode numbers (episode → season → show walk).
            let mut stmt = conn.prepare(
                "SELECT i.id, i.kind, i.tmdb_id, i.season_number, i.episode_number,
                        sh.tmdb_id,
                        w.position_ms, w.duration_ms, w.watched, w.updated_at,
                        (SELECT f.duration_ms FROM files f
                          WHERE f.item_id = i.id AND f.duration_ms IS NOT NULL
                          LIMIT 1)
                 FROM items i
                 LEFT JOIN items se ON se.id = i.parent_id
                 LEFT JOIN items sh ON sh.id = se.parent_id
                 LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?1
                 WHERE i.kind IN ('movie','episode')",
            )?;
            let rows = stmt.query_map(params![user_id], |row| {
                let item_id: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                let own_tmdb: Option<i64> = row.get(2)?;
                let season: Option<i64> = row.get(3)?;
                let episode: Option<i64> = row.get(4)?;
                let show_tmdb: Option<i64> = row.get(5)?;
                let watch = match row.get::<_, Option<i64>>(6)? {
                    Some(position_ms) => Some(LocalWatch {
                        position_ms,
                        duration_ms: row.get(7)?,
                        watched: row.get::<_, i64>(8)? != 0,
                        updated_at: row.get(9)?,
                    }),
                    None => None,
                };
                let file_duration_ms: Option<i64> = row.get(10)?;
                let ident = match kind.as_str() {
                    "movie" => own_tmdb.map(|tmdb| Ident::Movie { tmdb }),
                    "episode" => match (show_tmdb, season, episode) {
                        (Some(show_tmdb), Some(s), Some(e)) => Some(Ident::Episode {
                            show_tmdb,
                            season: s as i32,
                            episode: e as i32,
                        }),
                        _ => None,
                    },
                    _ => None,
                };
                Ok(ident.map(|ident| SyncCandidate {
                    item_id,
                    ident,
                    watch,
                    file_duration_ms,
                }))
            })?;
            let mut out = Vec::new();
            for row in rows {
                if let Some(cand) = row? {
                    out.push(cand);
                }
            }
            Ok(out)
        })
        .await
    }
}
