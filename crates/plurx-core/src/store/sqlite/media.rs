//! Items (movie/show/season/episode), media files, search.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::{
    file_from_row, item_cols, item_from_row, SqliteStore, FILE_COLS, ITEM_COLS, ITEM_COL_COUNT,
};
use crate::domain::{
    sort_title_for, ArtworkAttempt, Item, ItemEdit, ItemKind, ItemPage, ItemSort, MediaFile,
    MetadataPatch, NewItem, ProbeResult, RecentItem,
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

/// An ` AND <column> IN (…)` fragment for an optional id narrowing.
///
/// `None` in gives an empty string out, so an unnarrowed query is the
/// original SQL character for character — the whole-library path must not
/// change shape because a targeted path wanted a filter. `Some(&[])` gives
/// `None` out, meaning "the caller asked for zero items": the caller returns
/// early rather than running a query that would match everything.
///
/// Inline rather than bound, like `child_counts`: these are our own row ids,
/// i64s that came out of this database, and a bound IN-list needs a
/// statement per arity.
fn id_filter(column: &str, only: Option<&[i64]>) -> Option<String> {
    match only {
        None => Some(String::new()),
        Some([]) => None,
        Some(ids) => {
            let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            Some(format!(" AND {column} IN ({list})"))
        }
    }
}

#[async_trait]
impl MediaStore for SqliteStore {
    async fn item_by_external_id(
        &self,
        kind: ItemKind,
        tmdb_id: Option<i64>,
        imdb_id: Option<&str>,
    ) -> Result<Option<Item>, StoreError> {
        // Neither id means nothing to match on. Falling through would compare
        // NULL against NULL and return the first row of that kind — a wrong
        // answer delivered confidently, which is the failure this whole
        // ids-only rule exists to prevent.
        let imdb_id = imdb_id.filter(|s| !s.is_empty()).map(str::to_owned);
        if tmdb_id.is_none() && imdb_id.is_none() {
            return Ok(None);
        }
        let kind = kind.as_str().to_owned();
        self.with_conn(move |conn| {
            // Each arm is guarded by its own `IS NOT NULL`, so an id we do not
            // have can never match a row whose id is also NULL. TMDB wins ties
            // because it is what plurx stores for everything it enriched; the
            // IMDb arm is the fallback for items adopted from an NFO.
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE kind = ?1
                       AND ((?2 IS NOT NULL AND tmdb_id = ?2)
                         OR (?3 IS NOT NULL AND imdb_id = ?3 COLLATE NOCASE))
                     ORDER BY (?2 IS NOT NULL AND tmdb_id = ?2) DESC, id
                     LIMIT 1"
                ),
                params![kind, tmdb_id, imdb_id],
            )?)
        })
        .await
    }

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

    async fn find_child_item(
        &self,
        library_id: i64,
        parent_id: Option<i64>,
        kind: ItemKind,
        title: &str,
    ) -> Result<Option<Item>, StoreError> {
        let title = title.to_owned();
        let kind = kind.as_str();
        self.with_conn(move |conn| {
            Ok(find_by(
                conn,
                &format!(
                    "SELECT {ITEM_COLS} FROM items
                     WHERE library_id = ?1 AND kind = ?3 AND title = ?4 COLLATE NOCASE
                       AND parent_id IS ?2"
                ),
                params![library_id, parent_id, kind, title],
            )?)
        })
        .await
    }

    async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError> {
        let item = item.clone();
        self.with_conn(move |conn| {
            // Folder titles are directory names, not work titles: "The Lake
            // House 2021" is a place. Stripping the article would file it
            // under L, so folders sort on the raw name.
            let sort_title = if item.kind == ItemKind::Folder {
                item.title.to_lowercase()
            } else {
                sort_title_for(&item.title)
            };
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
            // Shows order by season/episode; home folders want subfolders
            // first, then their media chronologically. The extra keys are
            // inert for movie/show libraries (recorded_at is NULL there).
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items WHERE parent_id = ?1
                 ORDER BY (kind = 'folder') DESC, season_number, episode_number,
                          (recorded_at IS NULL), recorded_at, sort_title"
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
                // Best (max) file height per item, highest first; no-height items last.
                ItemSort::Resolution => {
                    "COALESCE((SELECT MAX(f.height) FROM files f WHERE f.item_id = items.id), -1) DESC, sort_title ASC"
                }
                ItemSort::Recorded => "(recorded_at IS NULL), recorded_at DESC, sort_title ASC",
            };
            // Top level = what a library's grid shows: movies and shows, plus
            // (home libraries) whatever sits directly under a root.
            const TOP: &str = "(kind IN ('movie','show') \
                 OR (kind IN ('folder','video','photo') AND parent_id IS NULL))";
            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM items WHERE library_id = ?1 AND {TOP}"),
                params![library_id],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items
                 WHERE library_id = ?1 AND {TOP}
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
            // One card per movie, per show (latest episode represents the
            // show), and — in home libraries — per video or folder. SQLite's
            // bare-column-with-MAX picks that latest row. Photos are excluded
            // on purpose: a 2,000-photo import would otherwise flood the home
            // screen, and its videos and folders still surface it.
            let mut stmt = conn.prepare(&format!(
                "SELECT {i}, show.title, season.poster_path, MAX(i.added_at) AS latest
                 FROM items i
                 LEFT JOIN items season
                        ON season.id = i.parent_id AND i.kind = 'episode'
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE i.kind IN ('movie','episode','video','folder')
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
                        show_title: row.get(ITEM_COL_COUNT)?,
                        season_poster: row.get(ITEM_COL_COUNT + 1)?,
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
                "SELECT {i}, show.title, season.poster_path
                 FROM items_fts f
                 JOIN items i ON i.id = f.rowid
                 LEFT JOIN items season
                        ON season.id = i.parent_id AND i.kind = 'episode'
                 LEFT JOIN items show ON show.id = season.parent_id
                 WHERE items_fts MATCH ?1
                   AND i.kind IN ('movie','show','episode','folder','video','photo')
                 ORDER BY rank LIMIT ?2",
                i = item_cols("i")
            ))?;
            let items = stmt
                .query_map(params![match_expr, limit], |row| {
                    Ok(RecentItem {
                        item: item_from_row(row, 0)?,
                        show_title: row.get(ITEM_COL_COUNT)?,
                        season_poster: row.get(ITEM_COL_COUNT + 1)?,
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
            let tags = patch
                .tags
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| StoreError::Database(e.to_string()))?;
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
                     recorded_at = COALESCE(?12, recorded_at),
                     tags = COALESCE(?13, tags),
                     -- Sticky: once a provider has answered, a later patch
                     -- that does not claim to be enrichment (caller-supplied
                     -- ids, a Trakt backfill) must not un-mark the item.
                     metadata_at = CASE WHEN ?14 = 1 THEN unixepoch() ELSE metadata_at END,
                     -- Only a patch that actually tried to fetch artwork
                     -- speaks for these two. Everything else — a Trakt
                     -- backfill, a caller-supplied id — leaves the record of
                     -- the last attempt intact, so the retry sweep keeps its
                     -- backoff instead of being reset to zero by traffic that
                     -- has nothing to do with images.
                     artwork_attempted_at =
                         CASE WHEN ?15 = 1 THEN unixepoch() ELSE artwork_attempted_at END,
                     artwork_error = CASE WHEN ?15 = 1 THEN ?16 ELSE artwork_error END,
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
                    patch.recorded_at,
                    tags,
                    patch.enriched as i64,
                    patch.artwork.is_some() as i64,
                    match &patch.artwork {
                        Some(ArtworkAttempt::Failed(why)) => Some(why.as_str()),
                        _ => None,
                    },
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn update_item_fields(
        &self,
        item_id: i64,
        edit: &ItemEdit,
    ) -> Result<Option<Item>, StoreError> {
        let edit = edit.clone();
        self.with_conn(move |conn| {
            // Distinct from apply_metadata: an edit must be able to CLEAR a
            // field, so each column is guarded by a "was it in the request?"
            // flag rather than by the value being non-NULL.
            let sort_title = edit.title.as_deref().map(sort_title_for);
            let tags = edit
                .tags
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| StoreError::Database(e.to_string()))?;
            conn.execute(
                "UPDATE items SET
                     title      = CASE WHEN ?2 THEN ?3 ELSE title END,
                     sort_title = CASE WHEN ?2 THEN ?4 ELSE sort_title END,
                     overview   = CASE WHEN ?5 THEN ?6 ELSE overview END,
                     recorded_at = CASE WHEN ?7 THEN ?8 ELSE recorded_at END,
                     year       = CASE WHEN ?9 THEN ?10 ELSE year END,
                     tags       = CASE WHEN ?11 THEN ?12 ELSE tags END,
                     updated_at = unixepoch()
                 WHERE id = ?1",
                params![
                    item_id,
                    edit.title.is_some(),
                    edit.title,
                    sort_title,
                    edit.overview.is_some(),
                    edit.overview.flatten(),
                    edit.recorded_at.is_some(),
                    edit.recorded_at.flatten(),
                    edit.year.is_some(),
                    edit.year.flatten(),
                    tags.is_some(),
                    tags,
                ],
            )?;
            Ok(find_by(
                conn,
                &format!("SELECT {ITEM_COLS} FROM items WHERE id = ?1"),
                params![item_id],
            )?)
        })
        .await
    }

    async fn set_nfo_seeded(&self, item_id: i64) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE items SET nfo_seeded_at = unixepoch() WHERE id = ?1",
                params![item_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn items_needing_artwork(
        &self,
        library_id: i64,
        force: bool,
        only: Option<&[i64]>,
    ) -> Result<Vec<Item>, StoreError> {
        let Some(narrow) = id_filter("id", only) else {
            return Ok(Vec::new());
        };
        self.with_conn(move |conn| {
            // Folders last: a folder inherits its first child's poster, so the
            // children have to be thumbed first.
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLS} FROM items
                 WHERE library_id = ?1 AND kind IN ('folder','video','photo')
                   AND (?2 = 1 OR poster_path IS NULL){narrow}
                 ORDER BY (kind = 'folder'), id"
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

    async fn items_needing_metadata(
        &self,
        library_id: Option<i64>,
        force: bool,
        only: Option<&[i64]>,
    ) -> Result<Vec<Item>, StoreError> {
        let Some(narrow) = id_filter("i.id", only) else {
            return Ok(Vec::new());
        };
        self.with_conn(move |conn| {
            // Home libraries never see a provider: there is nothing to match
            // "Christmas 2019.mp4" against, and a false match would be worse
            // than nothing. Guarded here so no enrichment loop can reach them.
            let mut stmt = conn.prepare(&format!(
                "SELECT {i} FROM items i
                 JOIN libraries l ON l.id = i.library_id AND l.kind != 'home'
                 WHERE i.kind IN ('movie','show') AND (?2 = 1 OR i.metadata_at IS NULL)
                   AND (?1 IS NULL OR i.library_id = ?1){narrow}
                 ORDER BY i.id",
                i = item_cols("i")
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

    async fn items_missing_artwork(
        &self,
        library_id: Option<i64>,
        retry_after_secs: i64,
    ) -> Result<Vec<Item>, StoreError> {
        self.with_conn(move |conn| {
            // `metadata_at IS NOT NULL` is what makes this "the provider was
            // asked and the poster still isn't here" rather than "this item
            // has not been enriched yet" — the latter is the ordinary scan's
            // job and re-doing it here would race it.
            //
            // Only movies and shows: those are the kinds `enrich_library`
            // acts on, and selecting anything else would hand the sweep work
            // it cannot do, forever, with no attempt stamp written to make it
            // stop. Home libraries are absent for free (they never stamp
            // `metadata_at`) and need nothing here anyway — their artwork
            // query already keys on `poster_path IS NULL`, which is the
            // correct-by-construction version of the bug this repairs.
            let mut stmt = conn.prepare(&format!(
                "SELECT {i} FROM items i
                 JOIN libraries l ON l.id = i.library_id AND l.kind != 'home'
                 WHERE i.kind IN ('movie','show')
                   AND i.poster_path IS NULL
                   AND i.metadata_at IS NOT NULL
                   AND (i.artwork_attempted_at IS NULL
                        OR i.artwork_attempted_at <= unixepoch() - ?2)
                   AND (?1 IS NULL OR i.library_id = ?1)
                 ORDER BY i.artwork_attempted_at IS NOT NULL, i.artwork_attempted_at, i.id",
                i = item_cols("i")
            ))?;
            let items = stmt
                .query_map(params![library_id, retry_after_secs.max(0)], |row| {
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

    async fn merge_file_probe_chapters(
        &self,
        file_id: i64,
        chapters_json: &str,
    ) -> Result<(), StoreError> {
        let chapters = chapters_json.to_owned();
        self.with_conn(move |conn| {
            // `json_set` rather than read-modify-write in Rust: the whole
            // document is rewritten either way, but doing it in SQL keeps the
            // update atomic against a concurrent scan writing the same row.
            // Guarded on NOT NULL because json_set(NULL, …) is NULL, which
            // would erase the probe-failed marker `probe_json IS NULL` — the
            // one fingerprint the repair job keys on.
            conn.execute(
                "UPDATE files SET probe_json = json_set(probe_json, '$.chapters', json(?2))
                 WHERE id = ?1 AND probe_json IS NOT NULL",
                params![file_id, chapters],
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

    async fn files_missing_probe(
        &self,
        library_id: Option<i64>,
    ) -> Result<Vec<MediaFile>, StoreError> {
        self.with_conn(move |conn| {
            // Narrowed through a subquery rather than a join: `FILE_COLS` is
            // unqualified, and joining `items` would make `id` ambiguous.
            // Oldest scan first, so a capped retry pass works through the
            // backlog instead of re-trying the same few files forever.
            let mut stmt = conn.prepare(&format!(
                "SELECT {FILE_COLS} FROM files
                 WHERE probe_json IS NULL
                   AND (?1 IS NULL
                        OR item_id IN (SELECT id FROM items WHERE library_id = ?1))
                 ORDER BY scanned_at ASC, id ASC"
            ))?;
            let files = stmt
                .query_map(params![library_id], file_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(files)
        })
        .await
    }

    async fn child_counts(&self, ids: &[i64]) -> Result<HashMap<i64, i64>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // ids are our own row ids (trusted i64s), so an inline IN-list is safe.
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT parent_id, COUNT(*) FROM items
                 WHERE parent_id IN ({list}) GROUP BY parent_id"
            ))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    async fn item_max_heights(&self, ids: &[i64]) -> Result<HashMap<i64, i64>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // ids come from our own item rows (trusted i64s), so an inline IN-list
        // is safe and avoids a variadic-params dance.
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT item_id, MAX(height) FROM files
                 WHERE height IS NOT NULL AND item_id IN ({list})
                 GROUP BY item_id"
            ))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().collect())
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
                "DELETE FROM items
                 WHERE library_id = ?1 AND kind IN ('movie','episode','video','photo')
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
            // Folders nest arbitrarily deep, so one pass only strips the
            // innermost layer: delete a whole subtree of files and the empty
            // chain above it has to go too. Loop until a pass removes nothing.
            loop {
                let pass = tx.execute(
                    "DELETE FROM items WHERE library_id = ?1 AND kind = 'folder'
                     AND id NOT IN (SELECT parent_id FROM items
                                    WHERE parent_id IS NOT NULL)",
                    params![library_id],
                )? as u64;
                removed += pass;
                if pass == 0 {
                    break;
                }
            }
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
        ArtworkAttempt, AudioStream, ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem,
        NewLibrary, ProbeResult,
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

    async fn seed_movie_h(store: &SqliteStore, lib: i64, title: &str, height: i64) -> i64 {
        let id = store
            .insert_item(&NewItem {
                library_id: lib,
                kind: ItemKind::Movie,
                parent_id: None,
                title: title.into(),
                year: Some(2000),
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
                    width: Some(height * 16 / 9),
                    height: Some(height),
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        id
    }

    #[tokio::test]
    async fn resolution_sort_orders_by_height_desc() {
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
        seed_movie_h(&store, lib.id, "SD", 480).await;
        seed_movie_h(&store, lib.id, "UHD", 2160).await;
        seed_movie_h(&store, lib.id, "HD", 1080).await;
        // A movie whose file has no probed height sorts last (COALESCE -1).
        seed_movie(&store, lib.id, "Unknown", 2001).await;

        let page = store
            .list_top_items(lib.id, ItemSort::Resolution, 0, 10)
            .await
            .expect("list");
        let order: Vec<&str> = page.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(order, vec!["UHD", "HD", "SD", "Unknown"]);
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
    async fn home_library_items_round_trip() {
        use crate::domain::ItemEdit;

        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![PathBuf::from("/media/home")],
                anime: false,
            })
            .await
            .expect("lib");
        assert_eq!(lib.kind, LibraryKind::Home);

        let new_item = |kind, parent, title: &str| NewItem {
            library_id: lib.id,
            kind,
            parent_id: parent,
            title: title.to_owned(),
            year: None,
            season_number: None,
            episode_number: None,
        };
        let folder = store
            .insert_item(&new_item(ItemKind::Folder, None, "The Lake House 2021"))
            .await
            .expect("folder");
        let video = store
            .insert_item(&new_item(ItemKind::Video, Some(folder), "Christmas 2019"))
            .await
            .expect("video");
        let photo = store
            .insert_item(&new_item(ItemKind::Photo, Some(folder), "IMG_4021"))
            .await
            .expect("photo");

        // Folder titles keep their leading article — a folder is a place, not
        // a work, so "The Lake House 2021" must not file under L.
        let stored = store.get_item(folder).await.expect("get").expect("present");
        assert_eq!(stored.sort_title, "the lake house 2021");
        assert_eq!(stored.tags, Vec::<String>::new());
        assert_eq!(stored.nfo_seeded_at, None);

        // Folder identity is (library, parent, kind, title).
        let found = store
            .find_child_item(lib.id, None, ItemKind::Folder, "the lake house 2021")
            .await
            .expect("find")
            .expect("present");
        assert_eq!(found.id, folder);
        assert!(store
            .find_child_item(
                lib.id,
                Some(folder),
                ItemKind::Folder,
                "The Lake House 2021"
            )
            .await
            .expect("find")
            .is_none());

        // Seeding from an NFO goes through the ordinary metadata path.
        store
            .apply_metadata(
                video,
                &MetadataPatch {
                    title: Some("Beach day".into()),
                    recorded_at: Some("2019-06-14".into()),
                    tags: Some(vec!["beach".into(), "kids".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("seed");
        store.set_nfo_seeded(video).await.expect("mark seeded");
        let seeded = store.get_item(video).await.expect("get").expect("present");
        assert_eq!(seeded.recorded_at.as_deref(), Some("2019-06-14"));
        assert_eq!(seeded.tags, vec!["beach".to_owned(), "kids".to_owned()]);
        assert!(seeded.nfo_seeded_at.is_some());
        // Tags are searchable (they ride the rebuilt FTS index).
        let hits = store.search_items("kids", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item.id, video);

        // An edit can clear a field; a patch cannot.
        let edited = store
            .update_item_fields(
                video,
                &ItemEdit {
                    recorded_at: Some(None),
                    overview: Some(Some("Windy.".into())),
                    tags: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .expect("edit")
            .expect("present");
        assert_eq!(edited.recorded_at, None);
        assert_eq!(edited.overview.as_deref(), Some("Windy."));
        assert!(edited.tags.is_empty());
        assert_eq!(edited.title, "Beach day", "an absent field is untouched");

        // Top level = what sits under a root; children hang off the folder.
        let page = store
            .list_top_items(lib.id, ItemSort::Recorded, 0, 10)
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, folder);
        let kids = store.get_item_children(folder).await.expect("children");
        assert_eq!(kids.len(), 2);

        // Recorded sort: dated items first (newest first), undated last.
        store
            .apply_metadata(
                photo,
                &MetadataPatch {
                    recorded_at: Some("2019-06-15".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("date photo");
        let dated = store
            .list_top_items(lib.id, ItemSort::Recorded, 0, 10)
            .await
            .expect("list");
        assert_eq!(dated.items[0].id, folder);
        assert_eq!(dated.items[0].recorded_at, None);
    }

    #[tokio::test]
    async fn empty_folder_chains_prune_all_the_way_up() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![PathBuf::from("/media/home")],
                anime: false,
            })
            .await
            .expect("lib");
        let mut parent = None;
        for name in ["2019", "Summer", "Beach Trip"] {
            parent = Some(
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Folder,
                        parent_id: parent,
                        title: name.into(),
                        year: None,
                        season_number: None,
                        episode_number: None,
                    })
                    .await
                    .expect("folder"),
            );
        }
        let video = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Video,
                parent_id: parent,
                title: "clip".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("video");
        store
            .upsert_file(video, "/media/home/clip.mp4", 1, 1, &ProbeResult::default())
            .await
            .expect("file");

        assert_eq!(store.prune_empty_items(lib.id).await.expect("prune"), 0);

        // Delete the only file: the video and all three folders above it go.
        let paths = store.library_file_paths(lib.id).await.expect("paths");
        store.delete_files(&[paths[0].0]).await.expect("delete");
        assert_eq!(
            store.prune_empty_items(lib.id).await.expect("prune"),
            4,
            "a nested empty chain must prune in full, not one layer per scan"
        );
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
                .items_needing_metadata(None, false, None)
                .await
                .expect("needing")
                .len(),
            1
        );
        // An id on its own does NOT mean "enriched". This is the shape of
        // patch another application's scan request produces: it names what
        // the item is, which is a reason to go and fetch its metadata, not a
        // reason to consider it fetched.
        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    tmdb_id: Some(78),
                    ..Default::default()
                },
            )
            .await
            .expect("ids only");
        assert_eq!(
            store
                .items_needing_metadata(None, false, None)
                .await
                .expect("needing")
                .len(),
            1,
            "an item that arrived with an id still needs everything else"
        );

        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    title: Some("Blade Runner".into()),
                    overview: Some("A blade runner must pursue replicants.".into()),
                    tmdb_id: Some(78),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("patch");
        assert!(store
            .items_needing_metadata(None, false, None)
            .await
            .expect("needing")
            .is_empty());

        // And the mark is sticky: a later id-only patch (a Trakt backfill,
        // a re-announced import) must not push the item back into the queue.
        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    imdb_id: Some("tt0083658".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("later patch");
        assert!(store
            .items_needing_metadata(None, false, None)
            .await
            .expect("needing")
            .is_empty());

        // A forced refresh returns the already-matched item anyway (backfills
        // season posters onto shows enriched before that existed).
        assert_eq!(
            store
                .items_needing_metadata(None, true, None)
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

    /// The narrowing a targeted scan runs on. `None` must keep the query
    /// exactly as it was — the whole-library path is not allowed to change
    /// because a second caller wanted a filter.
    #[tokio::test]
    async fn items_needing_metadata_narrows_to_the_ids_asked_for() {
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
        let a = seed_movie(&store, lib.id, "Alien", 1979).await;
        let b = seed_movie(&store, lib.id, "Aliens", 1986).await;

        let all = store
            .items_needing_metadata(None, false, None)
            .await
            .expect("all");
        assert_eq!(all.len(), 2, "no filter is every candidate, as before");

        let one = store
            .items_needing_metadata(None, false, Some(&[a]))
            .await
            .expect("one");
        assert_eq!(one.iter().map(|i| i.id).collect::<Vec<_>>(), [a]);

        let both = store
            .items_needing_metadata(None, false, Some(&[a, b]))
            .await
            .expect("both");
        assert_eq!(both.len(), 2);

        // "These zero items" is not "every item" — the difference between a
        // scan that placed nothing and a scan that placed the library.
        assert!(store
            .items_needing_metadata(None, false, Some(&[]))
            .await
            .expect("none")
            .is_empty());
    }

    /// The retry sweep's input. The case that matters is the third one: an
    /// item attempted moments ago must be left alone, or a permanently
    /// art-less film is re-fetched on every cycle forever.
    #[tokio::test]
    async fn items_missing_artwork_finds_enriched_items_with_no_poster() {
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
        let blank = seed_movie(&store, lib.id, "Solaris", 1972).await;
        let posted = seed_movie(&store, lib.id, "Stalker", 1979).await;
        let unenriched = seed_movie(&store, lib.id, "Mirror", 1975).await;

        // Enriched, poster download failed: exactly the state one TMDB blip
        // used to leave behind, silently and permanently.
        store
            .apply_metadata(
                blank,
                &MetadataPatch {
                    title: Some("Solaris".into()),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Failed("download: status 429".into())),
                    ..Default::default()
                },
            )
            .await
            .expect("blank");
        store
            .apply_metadata(
                posted,
                &MetadataPatch {
                    title: Some("Stalker".into()),
                    poster_path: Some("2-poster.jpg".into()),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Stored),
                    ..Default::default()
                },
            )
            .await
            .expect("posted");

        // No backoff: the one item that was matched and has no picture.
        let due = store
            .items_missing_artwork(None, 0)
            .await
            .expect("missing artwork");
        assert_eq!(
            due.iter().map(|i| i.id).collect::<Vec<_>>(),
            [blank],
            "an item with a poster is done, and an item nobody has enriched \
             yet belongs to the scan, not to the sweep"
        );
        assert_eq!(
            due[0].artwork_error.as_deref(),
            Some("download: status 429"),
            "the reason survives, so an operator can tell a rate limit from a 404"
        );
        assert!(due[0].artwork_attempted_at.is_some());
        assert!(store.get_item(unenriched).await.expect("get").is_some());

        // With a backoff, the same item is skipped — it was attempted a
        // moment ago, and the point of recording that was to be able to wait.
        assert!(store
            .items_missing_artwork(None, 3600)
            .await
            .expect("backoff")
            .is_empty());
    }

    /// A patch that is not about artwork must not disturb the attempt record.
    /// A Trakt backfill landing between sweeps would otherwise reset the
    /// backoff and turn the daily retry into a per-write retry.
    #[tokio::test]
    async fn an_unrelated_patch_leaves_the_artwork_attempt_alone() {
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
        let id = seed_movie(&store, lib.id, "Stalker", 1979).await;
        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Failed("download: status 500".into())),
                    ..Default::default()
                },
            )
            .await
            .expect("attempt");
        let before = store.get_item(id).await.expect("get").expect("item");

        store
            .apply_metadata(
                id,
                &MetadataPatch {
                    imdb_id: Some("tt0079944".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("unrelated");
        let after = store.get_item(id).await.expect("get").expect("item");
        assert_eq!(after.artwork_attempted_at, before.artwork_attempted_at);
        assert_eq!(after.artwork_error.as_deref(), Some("download: status 500"));
    }
}
