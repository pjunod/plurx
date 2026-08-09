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
            let mut stmt = conn.prepare(&format!(
                "SELECT {AUTH_COLS} FROM trakt_auth ORDER BY user_id"
            ))?;
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
            conn.execute(
                "DELETE FROM trakt_auth WHERE user_id = ?1",
                params![user_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_trakt_auth_if_current(
        &self,
        user_id: i64,
        expected_refresh_token: &str,
    ) -> Result<bool, StoreError> {
        let expected_refresh_token = expected_refresh_token.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "DELETE FROM trakt_auth WHERE user_id = ?1 AND refresh_token = ?2",
                params![user_id, expected_refresh_token],
            )? > 0)
        })
        .await
    }

    async fn update_trakt_tokens(
        &self,
        user_id: i64,
        expected_refresh_token: &str,
        access_token: &str,
        refresh_token: &str,
        expires_at: i64,
    ) -> Result<bool, StoreError> {
        let (expected_refresh_token, access_token, refresh_token) = (
            expected_refresh_token.to_owned(),
            access_token.to_owned(),
            refresh_token.to_owned(),
        );
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE trakt_auth SET access_token = ?2, refresh_token = ?3, expires_at = ?4
                 WHERE user_id = ?1 AND refresh_token = ?5",
                params![
                    user_id,
                    access_token,
                    refresh_token,
                    expires_at,
                    expected_refresh_token
                ],
            )? > 0)
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

    async fn trakt_sync_candidates(&self, user_id: i64) -> Result<Vec<SyncCandidate>, StoreError> {
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

#[cfg(test)]
mod tests {
    use crate::domain::{
        ItemKind, LibraryKind, MetadataPatch, NewItem, NewLibrary, ProbeResult, TraktAuth,
    };
    use crate::store::{LibraryStore, MediaStore, SqliteStore, TraktStore, UserStore, WatchStore};

    fn auth(user_id: i64) -> TraktAuth {
        TraktAuth {
            user_id,
            access_token: "acc".into(),
            refresh_token: "ref".into(),
            expires_at: 1000,
            trakt_username: Some("me".into()),
            connected_at: 500,
            last_sync_at: 0,
            last_activities: None,
        }
    }

    #[tokio::test]
    async fn auth_crud_roundtrip() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");

        // Nothing linked yet.
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
        assert!(store.list_trakt_auth().await.expect("list").is_empty());

        // Insert, then read back every column.
        store.put_trakt_auth(&auth(user.id)).await.expect("put");
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(got.access_token, "acc");
        assert_eq!(got.trakt_username.as_deref(), Some("me"));
        assert_eq!(got.connected_at, 500);
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);

        // Upsert on conflict: connected_at changes, tokens replaced.
        let mut updated = auth(user.id);
        updated.access_token = "acc2".into();
        updated.connected_at = 777;
        store.put_trakt_auth(&updated).await.expect("upsert");
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(got.access_token, "acc2");
        assert_eq!(got.connected_at, 777);
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);

        // Token refresh path updates only the token triple + expiry.
        assert!(store
            .update_trakt_tokens(user.id, "ref", "acc3", "ref3", 2000)
            .await
            .expect("tokens"));
        assert!(!store
            .update_trakt_tokens(user.id, "ref", "loser", "loser-ref", 3000)
            .await
            .expect("stale tokens"));
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(got.access_token, "acc3");
        assert_eq!(got.refresh_token, "ref3");
        assert_eq!(got.expires_at, 2000);

        // Sync bookkeeping.
        store
            .set_trakt_sync(user.id, 4242, Some("{\"all\":1}"))
            .await
            .expect("sync");
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(got.last_sync_at, 4242);
        assert_eq!(got.last_activities.as_deref(), Some("{\"all\":1}"));

        // Delete removes the row.
        store.delete_trakt_auth(user.id).await.expect("del");
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn sync_candidates_cover_movie_and_episode() {
        use crate::trakt::Ident;
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");

        // Movie with a TMDB id, a file (duration), and an in-progress watch.
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
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(603),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        store
            .upsert_file(
                movie,
                "/m/Heat.mkv",
                1,
                1,
                &ProbeResult {
                    duration_ms: Some(6_000_000),
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        store
            .put_progress(user.id, movie, 1_000_000, Some(6_000_000))
            .await
            .expect("progress");

        // Show → season → episode; the show carries the TMDB id.
        let show = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Show".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    tmdb_id: Some(42),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        let season = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "S1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let ep = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "S1E2".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(2),
            })
            .await
            .expect("ep");

        let cands = store
            .trakt_sync_candidates(user.id)
            .await
            .expect("candidates");
        // Movie + episode are eligible; the season row is not (not movie/episode).
        let movie_c = cands
            .iter()
            .find(|c| c.item_id == movie)
            .expect("movie candidate");
        assert_eq!(movie_c.ident, Ident::Movie { tmdb: 603 });
        assert_eq!(movie_c.file_duration_ms, Some(6_000_000));
        assert!(movie_c.watch.is_some_and(|w| w.position_ms == 1_000_000));

        let ep_c = cands
            .iter()
            .find(|c| c.item_id == ep)
            .expect("episode candidate");
        assert_eq!(
            ep_c.ident,
            Ident::Episode {
                show_tmdb: 42,
                season: 1,
                episode: 2,
            }
        );
        // The episode has no file and no watch row yet.
        assert!(ep_c.watch.is_none());
        assert!(ep_c.file_duration_ms.is_none());
    }
}
