//! Items (movie/show/season/episode), media files, search.

use std::path::PathBuf;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::{file_from_row, item_cols, item_from_row, SqliteStore, FILE_COLS, ITEM_COLS};
use crate::domain::{
    sort_title_for, Item, ItemPage, ItemSort, MediaFile, MetadataPatch, NewItem, ProbeResult,
    RecentItem,
};
use crate::error::StoreError;
use crate::store::MediaStore;

/// Build an FTS5 MATCH expression from free text: quoted tokens, prefix
/// matching on the last one. Returns `None` for queries with no tokens.
fn fts_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == last {
                    format!("\"{t}\"*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn find_by(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Option<Item>> {
    conn.query_row(sql, params, |row| item_from_row(row, 0))
        .optional()
}

#[async_trait]
impl MediaStore for SqliteStore {
    async fn find_movie(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError> {
        let title = title.to_owned();
        self.with_conn(move |conn| {
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE library_id = ?1 AND kind = 'movie'
                       AND title = ?2 COLLATE NOCASE AND year IS ?3"
                ),
                params![library_id, title, year],
            )?)
        })
        .await
    }

    async fn find_show(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError> {
        let title = title.to_owned();
        self.with_conn(move |conn| {
            // A show matches by title; year disambiguates only when both sides
            // have one (scanners often lack the year the second time around).
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE library_id = ?1 AND kind = 'show'
                       AND title = ?2 COLLATE NOCASE
                       AND (?3 IS NULL OR year IS NULL OR year = ?3)
                     ORDER BY (year = ?3) DESC"
                ),
                params![library_id, title, year],
            )?)
        })
        .await
    }

    async fn find_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<Item>, StoreError> {
        self.with_conn(move |conn| {
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE parent_id = ?1 AND kind = 'season' AND season_number = ?2"
                ),
                params![show_id, season_number],
            )?)
        })
        .await
    }

    async fn find_episode(
        &self,
        season_id: i64,
        episode_number: i32,
    ) -> Result<Option<Item>, StoreError> {
        self.with_conn(move |conn| {
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE parent_id = ?1 AND kind = 'episode' AND episode_number = ?2"
                ),
                params![season_id, episode_number],
            )?)
        })
        .await
    }

    async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError> {
        let item = item.clone();
        self.with_conn(move |conn| {
            let sort_title = sort_title_for(&item.title);
            conn.execute(
                "INSERT INTO items
                   (library_id, kind, parent_id, title, sort_title, year,
                    season_number, episode_number)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    item.library_id,
                    item.kind.as_str(),
                    item.parent_id,
                    item.title,
                    sort_title,
                    item.year,
                    item.season_number,
                    item.episode_number,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    async fn get_item(&self, id: i64) -> Result<Option<Item>, StoreError> {
        self.with_conn(move |conn| {
            Ok(find_by(
                conn,
                &format!("SELECT {ITEM_COLS} FROM items WHERE id = ?1"),
                params![id],
            )?)
        })
        .await
    }

    async fn get_item_children(&self, parent_id: i64) -> Result<Vec<Item>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items WHERE parent_id = ?1
                 ORDER BY season_number, episode_number, sort_title"
            ))?;
            let items = stmt
                .query_map(params![parent_id], |row| item_from_row(row, 0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(items)
        })
        .await
    }

    async fn list_top_items(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
    ) -> Result<ItemPage, StoreError> {
        self.with_conn(move |conn| {
            let order = match sort {
                ItemSort::Title => "sort_title ASC",
                ItemSort::Added => "added_at DESC, id DESC",
                ItemSort::Year => "year IS NULL, year DESC, sort_title ASC",
            };
            let total: i64 = conn.query_row(
                "SELECT COUNT(*) FROM items
                 WHERE library_id = ?1 AND kind IN ('movie','show')",
                params![library_id],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items
                 WHERE library_id = ?1 AND kind IN ('movie','show')
                 ORDER BY {order} LIMIT ?3 OFFSET ?2"
            ))?;
            let items = stmt
                .query_map(params![library_id, offset, limit], |row| {
                    item_from_row(row, 0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ItemPage { items, total })
        })
        .await
    }

    async fn recently_added(
        &self,
        library_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RecentItem>, StoreError> {
        self.with_conn(move |conn| {
            // One card per movie or per show (latest episode represents the
            // show). SQLite's bare-column-with-MAX picks that latest row.
            let mut stmt = conn.prepare(&format!(
                "SELECT {i}, show.title, MAX(i.added_at) AS latest
                 FROM items i
                 LEFT JOIN items season ON season.id = i.parent_id
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE i.kind IN ('movie','episode')
                   AND (?1 IS NULL OR i.library_id = ?1)
                 GROUP BY CASE WHEN i.kind = 'episode' THEN 'show:' || show.id
                               ELSE 'item:' || i.id END
                 ORDER BY latest DESC LIMIT ?2",
                i = item_cols("i")
            ))?;
            let items = stmt
                .query_map(params![library_id, limit], |row| {
                    Ok(RecentItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(18)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(items)
        })
        .await
    }

    async fn search_items(&self, query: &str, limit: i64) -> Result<Vec<RecentItem>, StoreError> {
        let Some(match_expr) = fts_query(query) else {
            return Ok(Vec::new());
        };
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {i}, show.title
                 FROM items_fts f
                 JOIN items i ON i.id = f.rowid
                 LEFT JOIN items season
                        ON season.id = i.parent_id AND i.kind = 'episode'
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE items_fts MATCH ?1 AND i.kind IN ('movie','show','episode')
                 ORDER BY rank LIMIT ?2",
                i = item_cols("i")
            ))?;
            let items = stmt
                .query_map(params![match_expr, limit], |row| {
                    Ok(RecentItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(18)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(items)
        })
        .await
    }

    async fn apply_metadata(&self, item_id: i64, patch: &MetadataPatch) -> Result<(), StoreError> {
        let patch = patch.clone();
        self.with_conn(move |conn| {
            let sort_title = patch.title.as_deref().map(sort_title_for);
            conn.execute(
                "UPDATE items SET
                     title = COALESCE(?2, title),
                     sort_title = COALESCE(?3, sort_title),
                     year = COALESCE(?4, year),
                     overview = COALESCE(?5, overview),
                     tmdb_id = COALESCE(?6, tmdb_id),
                     imdb_id = COALESCE(?7, imdb_id),
                     air_date = COALESCE(?8, air_date),
                     runtime_ms = COALESCE(?9, runtime_ms),
                     poster_path = COALESCE(?10, poster_path),
                     backdrop_path = COALESCE(?11, backdrop_path),
                     updated_at = unixepoch()
                 WHERE id = ?1",
                params![
                    item_id,
                    patch.title,
                    sort_title,
                    patch.year,
                    patch.overview,
                    patch.tmdb_id,
                    patch.imdb_id,
                    patch.air_date,
                    patch.runtime_ms,
                    patch.poster_path,
                    patch.backdrop_path,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn items_needing_metadata(
        &self,
        library_id: Option<i64>,
        force: bool,
    ) -> Result<Vec<Item>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items
                 WHERE kind IN ('movie','show') AND (?2 = 1 OR tmdb_id IS NULL)
                   AND (?1 IS NULL OR library_id = ?1)
                 ORDER BY id"
            ))?;
            let items = stmt
                .query_map(params![library_id, force as i64], |row| {
                    item_from_row(row, 0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(items)
        })
        .await
    }

    async fn episodes_for_show(&self, show_id: i64) -> Result<Vec<Item>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {e} FROM items e
                 JOIN items season ON e.parent_id = season.id
                 WHERE season.parent_id = ?1 AND e.kind = 'episode'
                 ORDER BY season.season_number, e.episode_number",
                e = item_cols("e")
            ))?;
            let items = stmt
                .query_map(params![show_id], |row| item_from_row(row, 0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(items)
        })
        .await
    }

    async fn get_file_by_path(&self, path: &str) -> Result<Option<MediaFile>, StoreError> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {FILE_COLS} FROM files WHERE path = ?1"),
                    params![path],
                    file_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn upsert_file(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
    ) -> Result<i64, StoreError> {
        let path = path.to_owned();
        let probe = probe.clone();
        self.with_conn(move |conn| {
            let audio = serde_json::to_string(&probe.audio_streams)
                .map_err(|e| StoreError::Database(e.to_string()))?;
            let subs = serde_json::to_string(&probe.subtitle_streams)
                .map_err(|e| StoreError::Database(e.to_string()))?;
            let id: i64 = conn.query_row(
                "INSERT INTO files
                   (item_id, path, size, mtime, duration_ms, container, video_codec,
                    video_profile, width, height, bit_depth, hdr, bitrate,
                    audio_streams, subtitle_streams, probe_json, hdr_format, scanned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, unixepoch())
                 ON CONFLICT(path) DO UPDATE SET
                     item_id = excluded.item_id,
                     size = excluded.size,
                     mtime = excluded.mtime,
                     duration_ms = excluded.duration_ms,
                     container = excluded.container,
                     video_codec = excluded.video_codec,
                     video_profile = excluded.video_profile,
                     width = excluded.width,
                     height = excluded.height,
                     bit_depth = excluded.bit_depth,
                     hdr = excluded.hdr,
                     bitrate = excluded.bitrate,
                     audio_streams = excluded.audio_streams,
                     subtitle_streams = excluded.subtitle_streams,
                     probe_json = excluded.probe_json,
                     hdr_format = excluded.hdr_format,
                     scanned_at = unixepoch()
                 RETURNING id",
                params![
                    item_id,
                    path,
                    size,
                    mtime,
                    probe.duration_ms,
                    probe.container,
                    probe.video_codec,
                    probe.video_profile,
                    probe.width,
                    probe.height,
                    probe.bit_depth,
                    probe.hdr,
                    probe.bitrate,
                    audio,
                    subs,
                    probe.raw_json,
                    probe.hdr_format,
                ],
                |row| row.get(0),
            )?;
            Ok(id)
        })
        .await
    }

    async fn get_file(&self, id: i64) -> Result<Option<MediaFile>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {FILE_COLS} FROM files WHERE id = ?1"),
                    params![id],
                    file_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn set_file_audio_offset(&self, file_id: i64, offset_ms: i64) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE files SET audio_offset_ms = ?2 WHERE id = ?1",
                params![file_id, offset_ms],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_file_probe_json(&self, file_id: i64) -> Result<Option<String>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT probe_json FROM files WHERE id = ?1",
                    params![file_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        })
        .await
    }

    async fn files_for_item(&self, item_id: i64) -> Result<Vec<MediaFile>, StoreError> {
        self.with_conn(move |conn| {
            // Best version first: an item can have several source files (a 4K
            // and a 1080p rip of the same movie). Order by resolution, then
            // bitrate, so clients default to the highest quality; SQLite
            // sorts NULLs last under DESC.
            let mut stmt = conn.prepare(&format!(
                "SELECT {FILE_COLS} FROM files WHERE item_id = ?1
                 ORDER BY height DESC, bitrate DESC, path"
            ))?;
            let files = stmt
                .query_map(params![item_id], file_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(files)
        })
        .await
    }

    async fn library_file_paths(&self, library_id: i64) -> Result<Vec<(i64, PathBuf)>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT f.id, f.path FROM files f
                 JOIN items i ON i.id = f.item_id
                 WHERE i.library_id = ?1",
            )?;
            let rows = stmt
                .query_map(params![library_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn delete_files(&self, ids: &[i64]) -> Result<u64, StoreError> {
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut deleted = 0u64;
            {
                let mut stmt = tx.prepare("DELETE FROM files WHERE id = ?1")?;
                for id in &ids {
                    deleted += stmt.execute(params![id])? as u64;
                }
            }
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    async fn prune_empty_items(&self, library_id: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut removed = 0u64;
            // Bottom-up: file-less leaves, then empty seasons, then empty shows.
            removed += tx.execute(
                "DELETE FROM items WHERE library_id = ?1 AND kind IN ('movie','episode')
                 AND id NOT IN (SELECT item_id FROM files)",
                params![library_id],
            )? as u64;
            removed += tx.execute(
                "DELETE FROM items WHERE library_id = ?1 AND kind = 'season'
                 AND id NOT IN (SELECT parent_id FROM items
                                WHERE kind = 'episode' AND parent_id IS NOT NULL)",
                params![library_id],
            )? as u64;
            removed += tx.execute(
                "DELETE FROM items WHERE library_id = ?1 AND kind = 'show'
                 AND id NOT IN (SELECT parent_id FROM items
                                WHERE kind = 'season' AND parent_id IS NOT NULL)",
                params![library_id],
            )? as u64;
            tx.commit()?;
            Ok(removed)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{
        AudioStream, ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem, NewLibrary,
        ProbeResult,
    };
    use crate::store::{LibraryStore, MediaStore, SqliteStore};

    async fn seed_movie(store: &SqliteStore, lib: i64, title: &str, year: i32) -> i64 {
        let id = store
            .insert_item(&NewItem {
                library_id: lib,
                kind: ItemKind::Movie,
                parent_id: None,
                title: title.into(),
                year: Some(year),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("insert");
        store
            .upsert_file(
                id,
                &format!("/media/{title}.mkv"),
                1000,
                1,
                &ProbeResult {
                    container: Some("mkv".into()),
                    video_codec: Some("h264".into()),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        channels: Some(2),
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        id
    }

    #[tokio::test]
    async fn movie_placement_files_and_browse() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/media")],
                anime: false,
            })
            .await
            .expect("lib");

        assert!(store
            .find_movie(lib.id, "Heat", Some(1995))
            .await
            .expect("find")
            .is_none());
        let id = seed_movie(&store, lib.id, "Heat", 1995).await;
        let found = store
            .find_movie(lib.id, "heat", Some(1995))
            .await
            .expect("find")
            .expect("present");
        assert_eq!(found.id, id);
        // Different year → different item.
        assert!(store
            .find_movie(lib.id, "Heat", Some(2024))
            .await
            .expect("find")
            .is_none());

        seed_movie(&store, lib.id, "The Matrix", 1999).await;
        let page = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(page.total, 2);
        // "The Matrix" sorts as "matrix" — after "heat".
        assert_eq!(page.items[0].title, "Heat");
        assert_eq!(page.items[1].title, "The Matrix");

        let files = store.files_for_item(id).await.expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].audio_streams[0].codec, "aac");

        // Unchanged file is recognized by path.
        let existing = store
            .get_file_by_path("/media/Heat.mkv")
            .await
            .expect("by path")
            .expect("present");
        assert_eq!(existing.size, 1000);
        assert_eq!(existing.mtime, 1);
    }

    #[tokio::test]
    async fn show_hierarchy_and_prune() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![PathBuf::from("/tv")],
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
        let ep = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "Good News About Hell".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("episode");
        store
            .upsert_file(
                ep,
                "/tv/severance-s01e01.mkv",
                5,
                5,
                &ProbeResult::default(),
            )
            .await
            .expect("file");

        let eps = store.episodes_for_show(show).await.expect("eps");
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].id, ep);

        let found = store
            .find_episode(season, 1)
            .await
            .expect("find")
            .expect("present");
        assert_eq!(found.id, ep);

        // Nothing to prune yet.
        assert_eq!(store.prune_empty_items(lib.id).await.expect("prune"), 0);

        // Remove the file → episode, season, and show all prune away.
        let paths = store.library_file_paths(lib.id).await.expect("paths");
        assert_eq!(paths.len(), 1);
        store
            .delete_files(&[paths[0].0])
            .await
            .expect("delete files");
        assert_eq!(store.prune_empty_items(lib.id).await.expect("prune"), 3);
        assert!(store.get_item(show).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn metadata_patch_and_search() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let id = seed_movie(&store, lib.id, "Blade Runer", 1982).await; // scanner typo

        assert_eq!(
            store
                .items_needing_metadata(None, false)
                .await
                .expect("needing")
                .len(),
            1
        );
        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    title: Some("Blade Runner".into()),
                    overview: Some("A blade runner must pursue replicants.".into()),
                    tmdb_id: Some(78),
                    ..Default::default()
                },
            )
            .await
            .expect("patch");
        assert!(store
            .items_needing_metadata(None, false)
            .await
            .expect("needing")
            .is_empty());

        // A forced refresh returns the already-matched item anyway (backfills
        // season posters onto shows enriched before that existed).
        assert_eq!(
            store
                .items_needing_metadata(None, true)
                .await
                .expect("forced")
                .len(),
            1
        );

        // FTS picks up the corrected title (trigger-synced), prefix search works.
        let hits = store.search_items("blade run", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item.title, "Blade Runner");
        // Overview matches too.
        let hits = store.search_items("replicants", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        // Garbage-only query is a no-op.
        assert!(store
            .search_items("  !!  ", 10)
            .await
            .expect("s")
            .is_empty());
    }
}
