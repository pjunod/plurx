//! Trakt account links and the sync-candidate join.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension, Row};

use super::SqliteStore;
use crate::domain::TraktAuth;
use crate::error::StoreError;
use crate::secrets::{CredentialKey, SealedRowCensus, SealedSecret};
use crate::store::{persistable_credential, TraktStore};
use crate::trakt::{Ident, LocalWatch, SyncCandidate};

fn auth_from_row(row: &Row<'_>) -> rusqlite::Result<TraktAuth> {
    Ok(TraktAuth {
        user_id: row.get(0)?,
        // Adopted verbatim: on a pre-encryption install these columns still
        // hold cleartext, and `migrate_trakt_credentials` is what turns them
        // into envelopes. Nothing here can read either form.
        access_token: SealedSecret::from_stored(row.get::<_, String>(1)?),
        refresh_token: SealedSecret::from_stored(row.get::<_, String>(2)?),
        expires_at: row.get(3)?,
        trakt_username: row.get(4)?,
        connected_at: row.get(5)?,
        last_sync_at: row.get(6)?,
        last_activities: row.get(7)?,
    })
}

const AUTH_COLS: &str = "user_id, access_token, refresh_token, expires_at, trakt_username, \
     connected_at, last_sync_at, last_activities";

/// Startup-time credential maintenance.
///
/// These are inherent methods rather than `TraktStore` calls on purpose. They
/// run once, before the store is serving anyone, and they are the only code in
/// plurx allowed to look at a Trakt credential column that might still be
/// cleartext.
impl SqliteStore {
    /// Describe the stored Trakt credentials that are already encrypted, and
    /// which keys they name.
    ///
    /// Answering this without the key is the point: it is what lets startup
    /// tell "first run, mint a key" from "the key file went missing" from "this
    /// is somebody else's key file", three situations that must not resolve the
    /// same way.
    pub async fn sealed_trakt_row_census(&self) -> Result<SealedRowCensus, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare("SELECT access_token, refresh_token FROM trakt_auth")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut census = SealedRowCensus::default();
            for (access, refresh) in rows {
                census.observe_row(
                    &SealedSecret::from_stored(access),
                    &SealedSecret::from_stored(refresh),
                );
            }
            Ok(census)
        })
        .await
    }

    /// Seal any Trakt credential still stored as cleartext, and report how many
    /// rows changed.
    ///
    /// Columns are examined one at a time so a process killed mid-migration
    /// resumes correctly: an already-sealed column is left exactly as it is,
    /// because re-sealing it would encrypt the envelope text and lose the
    /// credential inside. That resume property is also why this needs no
    /// explicit transaction — one statement seals both columns of a row, so no
    /// row is ever half-migrated, and a partial pass over the remaining rows is
    /// simply finished by the next boot.
    pub async fn migrate_trakt_credentials(
        &self,
        key: &CredentialKey,
    ) -> Result<usize, StoreError> {
        let key_id = key.id().to_owned();
        let pending = self
            .with_conn(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT user_id, access_token, refresh_token FROM trakt_auth")?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut updates: Vec<(i64, String, String)> = Vec::new();
        for (user_id, access, refresh) in pending {
            let access = SealedSecret::from_stored(access);
            let refresh = SealedSecret::from_stored(refresh);
            if access.looks_wrapped() && refresh.looks_wrapped() {
                continue;
            }
            let sealed_access = seal_unless_wrapped(key, user_id, &access)?;
            let sealed_refresh = seal_unless_wrapped(key, user_id, &refresh)?;
            updates.push((
                user_id,
                sealed_access.as_stored().to_owned(),
                sealed_refresh.as_stored().to_owned(),
            ));
        }
        if updates.is_empty() {
            return Ok(0);
        }

        let migrated = updates.len();
        self.with_conn(move |conn| {
            for (user_id, access, refresh) in &updates {
                conn.execute(
                    "UPDATE trakt_auth SET access_token = ?2, refresh_token = ?3
                     WHERE user_id = ?1",
                    params![user_id, access, refresh],
                )?;
            }
            Ok(())
        })
        .await?;

        // The count and the key id are the whole audit trail. Neither the
        // cleartext nor the ciphertext belongs in a log line.
        tracing::info!(
            rows = migrated,
            key_id = %key_id,
            "encrypted stored Trakt bearer credentials that were still cleartext"
        );
        Ok(migrated)
    }
}

/// Write a Trakt row the way a pre-encryption build did: both bearer columns
/// straight into SQL, in the clear.
///
/// Test-only, and raw SQL on purpose. `put_trakt_auth` now refuses an unsealed
/// credential, which is exactly the property under test — so the only honest way
/// to produce a legacy row is the way history actually produced one, going
/// around the boundary that did not exist yet.
#[cfg(test)]
impl SqliteStore {
    pub(crate) async fn seed_cleartext_trakt_auth(
        &self,
        user_id: i64,
        access_token: &str,
        refresh_token: &str,
    ) -> Result<(), StoreError> {
        let (access_token, refresh_token) = (access_token.to_owned(), refresh_token.to_owned());
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO trakt_auth
                   (user_id, access_token, refresh_token, expires_at, trakt_username,
                    connected_at, last_sync_at, last_activities)
                 VALUES (?1, ?2, ?3, 4000000000, 'legacy', 10, 0, NULL)
                 ON CONFLICT(user_id) DO UPDATE SET
                     access_token = excluded.access_token,
                     refresh_token = excluded.refresh_token",
                params![user_id, access_token, refresh_token],
            )?;
            Ok(())
        })
        .await
    }
}

fn seal_unless_wrapped(
    key: &CredentialKey,
    user_id: i64,
    value: &SealedSecret,
) -> Result<SealedSecret, StoreError> {
    if value.looks_wrapped() {
        return Ok(value.clone());
    }
    key.seal_trakt(user_id, value.as_stored())
        .map_err(|error| StoreError::Migration(format!("sealing Trakt credential: {error}")))
}

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
        // Before the row, not after: a durable write is the last place cleartext
        // could still get in, and once M2 replicates this row a mistake here is
        // in every voter's raft log where deleting the row cannot reach it.
        let access_token = persistable_credential(&auth.access_token)?;
        let refresh_token = persistable_credential(&auth.refresh_token)?;
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
                    access_token,
                    refresh_token,
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
        expected_refresh_token: &SealedSecret,
    ) -> Result<bool, StoreError> {
        let expected_refresh_token = expected_refresh_token.as_stored().to_owned();
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
        expected_refresh_token: &SealedSecret,
        access_token: &SealedSecret,
        refresh_token: &SealedSecret,
        expires_at: i64,
    ) -> Result<bool, StoreError> {
        // The expected operand is compared, never written, and a caller passes
        // back exactly what it read — including, on a store that has not been
        // upgraded yet, a cleartext column. Only the two values that land in the
        // row have to be envelopes.
        let (expected_refresh_token, access_token, refresh_token) = (
            expected_refresh_token.as_stored().to_owned(),
            persistable_credential(access_token)?,
            persistable_credential(refresh_token)?,
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
    use crate::error::StoreError;
    use crate::secrets::{CredentialKey, SealedSecret};
    use crate::store::{LibraryStore, MediaStore, SqliteStore, TraktStore, UserStore, WatchStore};

    /// The one key these tests seal under, so a stored envelope is comparable
    /// across a whole test rather than newly random each time it is mentioned.
    fn crud_key() -> CredentialKey {
        CredentialKey::from_bytes([0x44; 32])
    }

    fn auth(user_id: i64) -> TraktAuth {
        let key = crud_key();
        TraktAuth {
            user_id,
            access_token: key.seal_trakt(user_id, "acc").expect("seal access"),
            refresh_token: key.seal_trakt(user_id, "ref").expect("seal refresh"),
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
        let key = crud_key();

        // Nothing linked yet.
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
        assert!(store.list_trakt_auth().await.expect("list").is_empty());

        // Insert, then read back every column.
        let inserted = auth(user.id);
        store.put_trakt_auth(&inserted).await.expect("put");
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(
            got.access_token.as_stored(),
            inserted.access_token.as_stored()
        );
        assert_eq!(
            got.reveal_access_token(&key).expect("open").expose(),
            "acc",
            "the column round-trips the credential, not just the bytes"
        );
        assert_eq!(got.trakt_username.as_deref(), Some("me"));
        assert_eq!(got.connected_at, 500);
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);

        // Upsert on conflict: connected_at changes, tokens replaced.
        let mut updated = auth(user.id);
        updated.access_token = key.seal_trakt(user.id, "acc2").expect("seal");
        updated.connected_at = 777;
        store.put_trakt_auth(&updated).await.expect("upsert");
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(
            got.reveal_access_token(&key).expect("open").expose(),
            "acc2"
        );
        assert_eq!(got.connected_at, 777);
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);

        // Token refresh path updates only the token triple + expiry. The
        // compare-and-set operand is the exact envelope now in the row.
        let current_refresh = got.refresh_token.clone();
        let acc3 = key.seal_trakt(user.id, "acc3").expect("seal");
        let ref3 = key.seal_trakt(user.id, "ref3").expect("seal");
        assert!(store
            .update_trakt_tokens(user.id, &current_refresh, &acc3, &ref3, 2000)
            .await
            .expect("tokens"));
        assert!(!store
            .update_trakt_tokens(
                user.id,
                &current_refresh,
                &key.seal_trakt(user.id, "loser").expect("seal"),
                &key.seal_trakt(user.id, "loser-ref").expect("seal"),
                3000,
            )
            .await
            .expect("stale tokens"));
        let got = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(got.access_token.as_stored(), acc3.as_stored());
        assert_eq!(got.refresh_token.as_stored(), ref3.as_stored());
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

    /// A pre-encryption install: both bearer columns are plain text.
    async fn legacy_link(store: &SqliteStore, name: &str) -> i64 {
        let user = store.create_user(name, "hash", true).await.expect("user");
        store
            .seed_cleartext_trakt_auth(user.id, "plain-access", "plain-refresh")
            .await
            .expect("legacy link");
        user.id
    }

    #[tokio::test]
    async fn migration_seals_cleartext_and_then_has_nothing_left_to_do() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user_id = legacy_link(&store, "legacy").await;
        let key = CredentialKey::from_bytes([0x11; 32]);

        assert_eq!(
            store
                .sealed_trakt_row_census()
                .await
                .expect("count before")
                .sealed_rows(),
            0,
            "a cleartext row must not be mistaken for an encrypted one"
        );
        assert_eq!(
            store
                .migrate_trakt_credentials(&key)
                .await
                .expect("migrate"),
            1
        );

        let got = store
            .get_trakt_auth(user_id)
            .await
            .expect("get")
            .expect("some");
        assert!(got.is_wrapped());
        assert!(!got.access_token.as_stored().contains("plain-access"));
        assert_eq!(
            got.reveal_access_token(&key).expect("open").expose(),
            "plain-access"
        );
        assert_eq!(
            got.reveal_refresh_token(&key).expect("open").expose(),
            "plain-refresh"
        );
        assert_eq!(
            store
                .sealed_trakt_row_census()
                .await
                .expect("count after")
                .sealed_rows(),
            1
        );

        // Idempotent: re-sealing an envelope would encrypt the envelope text
        // and lose the credential inside it, so a second pass must do nothing.
        let sealed = got.access_token.as_stored().to_owned();
        assert_eq!(
            store.migrate_trakt_credentials(&key).await.expect("re-run"),
            0
        );
        assert_eq!(
            store
                .get_trakt_auth(user_id)
                .await
                .expect("get")
                .expect("some")
                .access_token
                .as_stored(),
            sealed
        );
    }

    /// A boot killed between the two columns of one row leaves a mixed row.
    /// The next pass must finish it without touching the column that already
    /// made it.
    #[tokio::test]
    async fn migration_resumes_a_row_whose_columns_disagree() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user_id = legacy_link(&store, "half").await;
        let key = CredentialKey::from_bytes([0x22; 32]);

        let sealed_access = key.seal_trakt(user_id, "plain-access").expect("seal");
        store
            .seed_cleartext_trakt_auth(user_id, sealed_access.as_stored(), "plain-refresh")
            .await
            .expect("half-migrated row");

        assert_eq!(
            store
                .sealed_trakt_row_census()
                .await
                .expect("count")
                .sealed_rows(),
            1,
            "a partly sealed row still needs the key that sealed it"
        );
        assert_eq!(
            store.migrate_trakt_credentials(&key).await.expect("resume"),
            1
        );

        let got = store
            .get_trakt_auth(user_id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(
            got.access_token.as_stored(),
            sealed_access.as_stored(),
            "the already-sealed column must be left exactly alone"
        );
        assert_eq!(
            got.reveal_refresh_token(&key).expect("open").expose(),
            "plain-refresh"
        );
    }

    /// The type is not the guarantee — inside this crate a `SealedSecret` can
    /// hold a pre-encryption column, and `from_stored` is how the row reader and
    /// the upgrade pass get one. The durable write is where that gets stopped,
    /// so a caller cannot route cleartext through the public `Store` trait.
    #[tokio::test]
    async fn a_store_call_cannot_persist_an_unsealed_credential() {
        let store = SqliteStore::open_in_memory().expect("open");
        let user = store.create_user("u", "h", true).await.expect("user");
        let key = crud_key();

        let mut cleartext = auth(user.id);
        cleartext.access_token = SealedSecret::from_stored("plain-bearer-token");
        let error = store
            .put_trakt_auth(&cleartext)
            .await
            .expect_err("an unsealed credential must not reach a durable row");
        assert!(
            matches!(error, StoreError::Credential(_)),
            "expected a credential refusal, got {error}"
        );
        assert!(
            !error.to_string().contains("plain-bearer-token"),
            "the refusal must not quote the credential it refused"
        );
        assert!(
            store.get_trakt_auth(user.id).await.expect("get").is_none(),
            "a refused write must leave no row behind"
        );

        // The rotation path refuses on the same terms, and leaves the row it
        // would have overwritten exactly as it was.
        let sealed = auth(user.id);
        store.put_trakt_auth(&sealed).await.expect("sealed put");
        let error = store
            .update_trakt_tokens(
                user.id,
                &sealed.refresh_token,
                &SealedSecret::from_stored("plain-rotated-access"),
                &key.seal_trakt(user.id, "rotated-refresh").expect("seal"),
                2000,
            )
            .await
            .expect_err("an unsealed rotation must not reach a durable row");
        assert!(matches!(error, StoreError::Credential(_)));
        let untouched = store
            .get_trakt_auth(user.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(
            untouched.access_token.as_stored(),
            sealed.access_token.as_stored()
        );
        assert_eq!(untouched.expires_at, sealed.expires_at);
    }

    #[tokio::test]
    async fn nothing_linked_means_nothing_to_migrate_and_nothing_wrapped() {
        let store = SqliteStore::open_in_memory().expect("open");
        let key = CredentialKey::from_bytes([0x33; 32]);

        assert_eq!(
            store
                .sealed_trakt_row_census()
                .await
                .expect("count")
                .sealed_rows(),
            0
        );
        assert_eq!(
            store
                .migrate_trakt_credentials(&key)
                .await
                .expect("migrate"),
            0
        );
    }
}
