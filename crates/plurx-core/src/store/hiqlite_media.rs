//! Replicated media catalogue implementation.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;

use super::hiqlite::{database_error, validate_sql, HiqliteAuthStore};
use super::{MediaStore, ReconcileOutcome, RootFingerprintStatus, WatchStore};
use crate::domain::{
    sort_title_for, ArtworkAttempt, InProgressItem, Item, ItemEdit, ItemKind, ItemPage, ItemSort,
    MediaFile, MediaShape, MetadataPatch, NewItem, ProbeResult, RecentItem, WatchRollup,
    WatchState,
};
use crate::error::StoreError;
use crate::mediafacts::{FactsRow, MediaFacts};

const ITEM_COLS: &str = "id, library_id, kind, parent_id, title, sort_title, year, overview, \
     tmdb_id, imdb_id, season_number, episode_number, air_date, runtime_ms, \
     poster_path, backdrop_path, added_at, updated_at, recorded_at, tags, nfo_seeded_at, \
     artwork_attempted_at, artwork_error, genres";

fn item_cols(alias: &str) -> String {
    ITEM_COLS
        .split(", ")
        .map(|column| format!("{alias}.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug)]
struct ItemRow {
    id: i64,
    library_id: i64,
    kind: String,
    parent_id: Option<i64>,
    title: String,
    sort_title: String,
    year: Option<i64>,
    overview: Option<String>,
    tmdb_id: Option<i64>,
    imdb_id: Option<String>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    air_date: Option<String>,
    runtime_ms: Option<i64>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    added_at: i64,
    updated_at: i64,
    recorded_at: Option<String>,
    tags: String,
    nfo_seeded_at: Option<i64>,
    artwork_attempted_at: Option<i64>,
    artwork_error: Option<String>,
    genres: String,
}

impl From<&mut Row<'_>> for ItemRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            library_id: row.get("library_id"),
            kind: row.get("kind"),
            parent_id: row.get("parent_id"),
            title: row.get("title"),
            sort_title: row.get("sort_title"),
            year: row.get("year"),
            overview: row.get("overview"),
            tmdb_id: row.get("tmdb_id"),
            imdb_id: row.get("imdb_id"),
            season_number: row.get("season_number"),
            episode_number: row.get("episode_number"),
            air_date: row.get("air_date"),
            runtime_ms: row.get("runtime_ms"),
            poster_path: row.get("poster_path"),
            backdrop_path: row.get("backdrop_path"),
            added_at: row.get("added_at"),
            updated_at: row.get("updated_at"),
            recorded_at: row.get("recorded_at"),
            tags: row.get("tags"),
            nfo_seeded_at: row.get("nfo_seeded_at"),
            artwork_attempted_at: row.get("artwork_attempted_at"),
            artwork_error: row.get("artwork_error"),
            genres: row.get("genres"),
        }
    }
}

impl TryFrom<ItemRow> for Item {
    type Error = StoreError;

    fn try_from(row: ItemRow) -> Result<Self, Self::Error> {
        let kind = ItemKind::parse(&row.kind)
            .ok_or_else(|| StoreError::Database(format!("unknown item kind `{}`", row.kind)))?;
        let tags = serde_json::from_str(&row.tags)
            .map_err(|error| StoreError::Database(format!("tags: {error}")))?;
        let genres = serde_json::from_str(&row.genres)
            .map_err(|error| StoreError::Database(format!("genres: {error}")))?;
        let year = optional_i32(row.year, "year")?;
        let season_number = optional_i32(row.season_number, "season_number")?;
        let episode_number = optional_i32(row.episode_number, "episode_number")?;
        Ok(Self {
            id: row.id,
            library_id: row.library_id,
            kind,
            parent_id: row.parent_id,
            title: row.title,
            sort_title: row.sort_title,
            year,
            overview: row.overview,
            tmdb_id: row.tmdb_id,
            imdb_id: row.imdb_id,
            season_number,
            episode_number,
            air_date: row.air_date,
            runtime_ms: row.runtime_ms,
            poster_path: row.poster_path,
            backdrop_path: row.backdrop_path,
            added_at: row.added_at,
            updated_at: row.updated_at,
            recorded_at: row.recorded_at,
            tags,
            nfo_seeded_at: row.nfo_seeded_at,
            artwork_attempted_at: row.artwork_attempted_at,
            artwork_error: row.artwork_error,
            genres,
        })
    }
}

fn optional_i32(value: Option<i64>, field: &str) -> Result<Option<i32>, StoreError> {
    value
        .map(|value| {
            i32::try_from(value).map_err(|_| {
                StoreError::Database(format!("{field} value {value} exceeds the i32 range"))
            })
        })
        .transpose()
}

struct RecentItemRow {
    item: ItemRow,
    show_title: Option<String>,
    season_poster: Option<String>,
}

impl From<&mut Row<'_>> for RecentItemRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item: ItemRow::from(&mut *row),
            show_title: row.get("rail_show_title"),
            season_poster: row.get("rail_season_poster"),
        }
    }
}

impl TryFrom<RecentItemRow> for RecentItem {
    type Error = StoreError;

    fn try_from(row: RecentItemRow) -> Result<Self, Self::Error> {
        Ok(Self {
            item: row.item.try_into()?,
            show_title: row.show_title,
            season_poster: row.season_poster,
        })
    }
}

struct CountRow {
    count: i64,
}

impl From<&mut Row<'_>> for CountRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            count: row.get("count"),
        }
    }
}

struct IdRow {
    id: i64,
}

const FILE_COLS: &str = "id, item_id, path, size, mtime, duration_ms, container, video_codec, \
     video_profile, width, height, bit_depth, hdr, bitrate, audio_streams, \
     subtitle_streams, scanned_at, hdr_format, audio_offset_ms, \
     (probe_json IS NOT NULL) AS probed";

struct FileRow {
    id: i64,
    item_id: i64,
    path: String,
    size: i64,
    mtime: i64,
    duration_ms: Option<i64>,
    container: Option<String>,
    video_codec: Option<String>,
    video_profile: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    bit_depth: Option<i64>,
    hdr: Option<String>,
    bitrate: Option<i64>,
    audio_streams: String,
    subtitle_streams: String,
    scanned_at: i64,
    hdr_format: Option<String>,
    audio_offset_ms: i64,
    probed: i64,
}

impl From<&mut Row<'_>> for FileRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            item_id: row.get("item_id"),
            path: row.get("path"),
            size: row.get("size"),
            mtime: row.get("mtime"),
            duration_ms: row.get("duration_ms"),
            container: row.get("container"),
            video_codec: row.get("video_codec"),
            video_profile: row.get("video_profile"),
            width: row.get("width"),
            height: row.get("height"),
            bit_depth: row.get("bit_depth"),
            hdr: row.get("hdr"),
            bitrate: row.get("bitrate"),
            audio_streams: row.get("audio_streams"),
            subtitle_streams: row.get("subtitle_streams"),
            scanned_at: row.get("scanned_at"),
            hdr_format: row.get("hdr_format"),
            audio_offset_ms: row.get("audio_offset_ms"),
            probed: row.get("probed"),
        }
    }
}

impl TryFrom<FileRow> for MediaFile {
    type Error = StoreError;

    fn try_from(row: FileRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            item_id: row.item_id,
            path: row.path.into(),
            size: row.size,
            mtime: row.mtime,
            duration_ms: row.duration_ms,
            container: row.container,
            video_codec: row.video_codec,
            video_profile: row.video_profile,
            width: row.width,
            height: row.height,
            bit_depth: row.bit_depth,
            hdr: row.hdr,
            hdr_format: row.hdr_format,
            bitrate: row.bitrate,
            audio_streams: serde_json::from_str(&row.audio_streams)
                .map_err(|error| StoreError::Database(format!("audio_streams: {error}")))?,
            subtitle_streams: serde_json::from_str(&row.subtitle_streams)
                .map_err(|error| StoreError::Database(format!("subtitle_streams: {error}")))?,
            scanned_at: row.scanned_at,
            audio_offset_ms: row.audio_offset_ms,
            probed: row.probed != 0,
        })
    }
}

struct ScalarRow {
    value: i64,
}

impl From<&mut Row<'_>> for ScalarRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

struct OptionalScalarRow {
    value: Option<i64>,
}

impl From<&mut Row<'_>> for OptionalScalarRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

struct PairRow {
    label: String,
    count: i64,
}

impl From<&mut Row<'_>> for PairRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            label: row.get("label"),
            count: row.get("count"),
        }
    }
}

struct ItemValueRow {
    item_id: i64,
    value: i64,
}

impl From<&mut Row<'_>> for ItemValueRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item_id: row.get("item_id"),
            value: row.get("value"),
        }
    }
}

struct FactsSqlRow {
    item_id: i64,
    files: i64,
    bytes: i64,
    container: Option<String>,
    video_codec: Option<String>,
    height: Option<i64>,
    hdr: Option<String>,
    hdr_format: Option<String>,
    audio_streams: String,
}

impl From<&mut Row<'_>> for FactsSqlRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item_id: row.get("item_id"),
            files: row.get("files"),
            bytes: row.get("bytes"),
            container: row.get("container"),
            video_codec: row.get("video_codec"),
            height: row.get("height"),
            hdr: row.get("hdr"),
            hdr_format: row.get("hdr_format"),
            audio_streams: row.get("audio_streams"),
        }
    }
}

struct PathRow {
    id: i64,
    path: String,
}

impl From<&mut Row<'_>> for PathRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            path: row.get("path"),
        }
    }
}

struct ProbeJsonRow {
    probe_json: Option<String>,
}

struct RootFingerprintRow {
    fingerprint: String,
}

impl From<&mut Row<'_>> for RootFingerprintRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            fingerprint: row.get("fingerprint"),
        }
    }
}

#[derive(Clone, Copy)]
struct WatchRow {
    position_ms: i64,
    duration_ms: Option<i64>,
    watched: i64,
    updated_at: i64,
}

impl From<&mut Row<'_>> for WatchRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            position_ms: row.get("position_ms"),
            duration_ms: row.get("duration_ms"),
            watched: row.get("watched"),
            updated_at: row.get("updated_at"),
        }
    }
}

impl From<WatchRow> for WatchState {
    fn from(row: WatchRow) -> Self {
        Self {
            position_ms: row.position_ms,
            duration_ms: row.duration_ms,
            watched: row.watched != 0,
            updated_at: row.updated_at,
        }
    }
}

struct WatchMapRow {
    item_id: i64,
    state: WatchRow,
}

impl From<&mut Row<'_>> for WatchMapRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item_id: row.get("item_id"),
            state: WatchRow::from(&mut *row),
        }
    }
}

struct RollupRow {
    root: i64,
    leaves: i64,
    watched: i64,
}

impl From<&mut Row<'_>> for RollupRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            root: row.get("root"),
            leaves: row.get("leaves"),
            watched: row.get("watched"),
        }
    }
}

struct InProgressRow {
    item: ItemRow,
    show_title: Option<String>,
    season_poster: Option<String>,
    state: WatchRow,
}

impl From<&mut Row<'_>> for InProgressRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item: ItemRow::from(&mut *row),
            show_title: row.get("rail_show_title"),
            season_poster: row.get("rail_season_poster"),
            state: WatchRow {
                position_ms: row.get("watch_position_ms"),
                duration_ms: row.get("watch_duration_ms"),
                watched: row.get("watch_watched"),
                updated_at: row.get("watch_updated_at"),
            },
        }
    }
}

impl TryFrom<InProgressRow> for InProgressItem {
    type Error = StoreError;

    fn try_from(row: InProgressRow) -> Result<Self, Self::Error> {
        Ok(Self {
            item: row.item.try_into()?,
            show_title: row.show_title,
            season_poster: row.season_poster,
            state: row.state.into(),
        })
    }
}

impl From<&mut Row<'_>> for ProbeJsonRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            probe_json: row.get("probe_json"),
        }
    }
}

impl From<&mut Row<'_>> for IdRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self { id: row.get("id") }
    }
}

fn items(rows: Vec<ItemRow>) -> Result<Vec<Item>, StoreError> {
    rows.into_iter().map(TryInto::try_into).collect()
}

fn one_item(rows: Vec<ItemRow>) -> Result<Option<Item>, StoreError> {
    rows.into_iter().next().map(TryInto::try_into).transpose()
}

fn files(rows: Vec<FileRow>) -> Result<Vec<MediaFile>, StoreError> {
    rows.into_iter().map(TryInto::try_into).collect()
}

fn one_file(rows: Vec<FileRow>) -> Result<Option<MediaFile>, StoreError> {
    rows.into_iter().next().map(TryInto::try_into).transpose()
}

fn recent_items(rows: Vec<RecentItemRow>) -> Result<Vec<RecentItem>, StoreError> {
    rows.into_iter().map(TryInto::try_into).collect()
}

fn fts_query(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
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
            .map(|(index, token)| {
                if index == last {
                    format!("\"{token}\"*")
                } else {
                    format!("\"{token}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn id_filter(column: &str, only: Option<&[i64]>) -> Option<String> {
    match only {
        None => Some(String::new()),
        Some([]) => None,
        Some(ids) => Some(format!(
            " AND {column} IN ({})",
            ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        )),
    }
}

fn id_list(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

async fn scalar(client: &hiqlite::Client, sql: &'static str) -> Result<i64, StoreError> {
    scalar_with(client, sql.to_owned(), params!()).await
}

async fn scalar_with(
    client: &hiqlite::Client,
    sql: String,
    params: hiqlite::Params,
) -> Result<i64, StoreError> {
    client
        .query_consistent_map::<ScalarRow, _>(sql, params)
        .await
        .map_err(database_error)?
        .into_iter()
        .next()
        .map(|row| row.value)
        .ok_or_else(|| StoreError::Database("aggregate query returned no row".to_owned()))
}

async fn pairs(
    client: &hiqlite::Client,
    sql: &'static str,
) -> Result<Vec<(String, i64)>, StoreError> {
    let mut values: Vec<_> = client
        .query_consistent_map::<PairRow, _>(sql, params!())
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|row| (row.label, row.count))
        .collect();
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    Ok(values)
}

#[async_trait]
impl MediaStore for HiqliteAuthStore {
    async fn item_by_external_id(
        &self,
        kind: ItemKind,
        tmdb_id: Option<i64>,
        imdb_id: Option<&str>,
    ) -> Result<Option<Item>, StoreError> {
        let imdb_id = imdb_id.filter(|value| !value.is_empty());
        if tmdb_id.is_none() && imdb_id.is_none() {
            return Ok(None);
        }
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE kind = $1 \
                         AND (($2 IS NOT NULL AND tmdb_id = $2) \
                         OR ($3 IS NOT NULL AND imdb_id = $3 COLLATE NOCASE)) \
                         ORDER BY ($2 IS NOT NULL AND tmdb_id = $2) DESC, id LIMIT 1"
                    ),
                    params!(kind.as_str(), tmdb_id, imdb_id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn find_movie(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE library_id = $1 \
                         AND kind = 'movie' AND title = $2 COLLATE NOCASE AND year IS $3"
                    ),
                    params!(library_id, title, year),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn find_show(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE library_id = $1 \
                         AND kind = 'show' AND title = $2 COLLATE NOCASE \
                         AND ($3 IS NULL OR year IS NULL OR year = $3) \
                         ORDER BY (year = $3) DESC"
                    ),
                    params!(library_id, title, year),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn find_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE parent_id = $1 \
                         AND kind = 'season' AND season_number = $2"
                    ),
                    params!(show_id, season_number),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn find_episode(
        &self,
        season_id: i64,
        episode_number: i32,
    ) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE parent_id = $1 \
                         AND kind = 'episode' AND episode_number = $2"
                    ),
                    params!(season_id, episode_number),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn find_child_item(
        &self,
        library_id: i64,
        parent_id: Option<i64>,
        kind: ItemKind,
        title: &str,
    ) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE library_id = $1 \
                         AND parent_id IS $2 AND kind = $3 AND title = $4 COLLATE NOCASE"
                    ),
                    params!(library_id, parent_id, kind.as_str(), title),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError> {
        let sort_title = if item.kind == ItemKind::Folder {
            item.title.to_lowercase()
        } else {
            sort_title_for(&item.title)
        };
        let now = self.now()?;
        let sql = "INSERT INTO items \
                   (library_id, kind, parent_id, title, sort_title, year, \
                    season_number, episode_number, added_at, updated_at) \
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING id";
        validate_sql(sql)?;
        let row = self
            .client
            .execute_returning_map_one::<_, IdRow>(
                sql,
                params!(
                    item.library_id,
                    item.kind.as_str(),
                    item.parent_id,
                    item.title.as_str(),
                    sort_title,
                    item.year,
                    item.season_number,
                    item.episode_number,
                    now
                ),
            )
            .await
            .map_err(database_error)?;
        Ok(row.id)
    }

    async fn get_item(&self, id: i64) -> Result<Option<Item>, StoreError> {
        one_item(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!("SELECT {ITEM_COLS} FROM items WHERE id = $1"),
                    params!(id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn get_item_children(&self, parent_id: i64) -> Result<Vec<Item>, StoreError> {
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items WHERE parent_id = $1 \
                         ORDER BY (kind = 'folder') DESC, season_number, episode_number, \
                         (recorded_at IS NULL), recorded_at, sort_title"
                    ),
                    params!(parent_id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn list_top_items_in_genre(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
        genre: Option<&str>,
    ) -> Result<ItemPage, StoreError> {
        let order = match sort {
            ItemSort::Title => "sort_title ASC",
            ItemSort::Added => "added_at DESC, id DESC",
            ItemSort::Year => "year IS NULL, year DESC, sort_title ASC",
            ItemSort::Resolution => {
                "COALESCE((SELECT MAX(f.height) FROM files f WHERE f.item_id = items.id), -1) DESC, sort_title ASC"
            }
            ItemSort::Recorded => "(recorded_at IS NULL), recorded_at DESC, sort_title ASC",
        };
        const TOP: &str = "(kind IN ('movie','show') OR \
             (kind IN ('folder','video','photo') AND parent_id IS NULL))";
        const GENRE: &str = "($2 IS NULL OR EXISTS (SELECT 1 FROM json_each(items.genres) \
             WHERE value = $2 COLLATE NOCASE))";
        let count = self
            .client
            .query_consistent_map::<CountRow, _>(
                format!(
                    "SELECT COUNT(*) AS count FROM items \
                     WHERE library_id = $1 AND {TOP} AND {GENRE}"
                ),
                params!(library_id, genre),
            )
            .await
            .map_err(database_error)?;
        let total = count
            .first()
            .ok_or_else(|| StoreError::Database("item count returned no row".to_owned()))?
            .count;
        let page_sql = format!(
            "SELECT {ITEM_COLS} FROM items WHERE library_id = $1 AND {TOP} AND {GENRE} \
             ORDER BY {order} LIMIT $3 OFFSET $4"
        );
        validate_sql(&page_sql)?;
        let page = items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    page_sql,
                    params!(library_id, genre, limit, offset),
                )
                .await
                .map_err(database_error)?,
        )?;
        Ok(ItemPage { items: page, total })
    }

    async fn recently_added(
        &self,
        library_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RecentItem>, StoreError> {
        let sql = format!(
            "WITH ranked AS ( \
                 SELECT {i}, show.title AS rail_show_title, \
                        season.poster_path AS rail_season_poster, \
                        ROW_NUMBER() OVER (PARTITION BY CASE \
                            WHEN i.kind = 'episode' AND show.id IS NOT NULL \
                            THEN 'show:' || show.id ELSE 'item:' || i.id END \
                            ORDER BY i.added_at DESC, COALESCE(season.season_number, -1) DESC, \
                            COALESCE(i.episode_number, -1) DESC, i.id DESC) AS rail_rank \
                 FROM items i \
                 LEFT JOIN items season ON season.id = i.parent_id AND i.kind = 'episode' \
                 LEFT JOIN items show ON show.id = season.parent_id \
                 WHERE i.kind IN ('movie','episode','video','folder') \
                   AND ($1 IS NULL OR i.library_id = $1) \
             ) \
             SELECT {r}, r.rail_show_title, r.rail_season_poster \
             FROM ranked r WHERE r.rail_rank = 1 \
             ORDER BY r.added_at DESC, r.id DESC LIMIT $2",
            i = item_cols("i"),
            r = item_cols("r")
        );
        recent_items(
            self.client
                .query_consistent_map::<RecentItemRow, _>(sql, params!(library_id, limit))
                .await
                .map_err(database_error)?,
        )
    }

    async fn search_items(&self, query: &str, limit: i64) -> Result<Vec<RecentItem>, StoreError> {
        let Some(match_expression) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {i}, show.title AS rail_show_title, \
                    season.poster_path AS rail_season_poster \
             FROM items_fts f JOIN items i ON i.id = f.rowid \
             LEFT JOIN items season ON season.id = i.parent_id AND i.kind = 'episode' \
             LEFT JOIN items show ON show.id = season.parent_id \
             WHERE items_fts MATCH $1 \
               AND i.kind IN ('movie','show','episode','folder','video','photo') \
             ORDER BY rank LIMIT $2",
            i = item_cols("i")
        );
        // Search is deliberately local derived-state I/O, unlike authoritative
        // catalogue reads. The three-node gate proves parity and one-node rebuild.
        recent_items(
            self.client
                .query_map::<RecentItemRow, _>(sql, params!(match_expression, limit))
                .await
                .map_err(database_error)?,
        )
    }

    async fn apply_metadata(&self, item_id: i64, patch: &MetadataPatch) -> Result<(), StoreError> {
        let sort_title = patch.title.as_deref().map(sort_title_for);
        let tags = patch
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(database_error)?;
        let genres = patch
            .genres
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(database_error)?;
        let artwork_error = match &patch.artwork {
            Some(ArtworkAttempt::Failed(reason)) => Some(reason.as_str()),
            _ => None,
        };
        let now = self.now()?;
        self.execute(
            "UPDATE items SET \
                 title = COALESCE($1, title), \
                 sort_title = COALESCE($2, sort_title), \
                 year = COALESCE($3, year), \
                 overview = COALESCE($4, overview), \
                 tmdb_id = COALESCE($5, tmdb_id), \
                 imdb_id = COALESCE($6, imdb_id), \
                 air_date = COALESCE($7, air_date), \
                 runtime_ms = COALESCE($8, runtime_ms), \
                 poster_path = COALESCE($9, poster_path), \
                 backdrop_path = COALESCE($10, backdrop_path), \
                 recorded_at = COALESCE($11, recorded_at), \
                 tags = COALESCE($12, tags), \
                 genres = COALESCE($13, genres), \
                 artwork_error = CASE WHEN $14 = 1 THEN $15 ELSE artwork_error END, \
                 metadata_at = CASE WHEN $16 = 1 THEN $17 ELSE metadata_at END, \
                 artwork_attempted_at = CASE WHEN $14 = 1 THEN $17 ELSE artwork_attempted_at END, \
                 updated_at = $17 \
             WHERE id = $18",
            params!(
                patch.title.as_deref(),
                sort_title,
                patch.year,
                patch.overview.as_deref(),
                patch.tmdb_id,
                patch.imdb_id.as_deref(),
                patch.air_date.as_deref(),
                patch.runtime_ms,
                patch.poster_path.as_deref(),
                patch.backdrop_path.as_deref(),
                patch.recorded_at.as_deref(),
                tags,
                genres,
                patch.artwork.is_some(),
                artwork_error,
                patch.enriched,
                now,
                item_id
            ),
        )
        .await?;
        Ok(())
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
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {i} FROM items i \
                         JOIN libraries l ON l.id = i.library_id AND l.kind != 'home' \
                         WHERE i.kind IN ('movie','show') \
                           AND ($1 = 1 OR i.metadata_at IS NULL) \
                           AND ($2 IS NULL OR i.library_id = $2){narrow} \
                         ORDER BY i.id",
                        i = item_cols("i")
                    ),
                    params!(force, library_id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn episodes_for_show(&self, show_id: i64) -> Result<Vec<Item>, StoreError> {
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {e} FROM items e JOIN items season ON e.parent_id = season.id \
                         WHERE season.parent_id = $1 AND e.kind = 'episode' \
                         ORDER BY season.season_number, e.episode_number",
                        e = item_cols("e")
                    ),
                    params!(show_id),
                )
                .await
                .map_err(database_error)?,
        )
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
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {ITEM_COLS} FROM items \
                         WHERE library_id = $1 AND kind IN ('folder','video','photo') \
                           AND ($2 = 1 OR poster_path IS NULL){narrow} \
                         ORDER BY (kind = 'folder'), id"
                    ),
                    params!(library_id, force),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn items_missing_artwork(
        &self,
        library_id: Option<i64>,
        retry_after_secs: i64,
        limit: i64,
    ) -> Result<Vec<Item>, StoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let cutoff = self.now()?.saturating_sub(retry_after_secs.max(0));
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {i} FROM items i \
                         JOIN libraries l ON l.id = i.library_id AND l.kind != 'home' \
                         LEFT JOIN items parent ON parent.id = i.parent_id \
                         LEFT JOIN items grandparent ON grandparent.id = parent.parent_id \
                         WHERE (i.poster_path IS NULL OR \
                                (i.kind IN ('movie','show') AND i.backdrop_path IS NULL)) \
                           AND ((i.kind IN ('movie','show') AND i.metadata_at IS NOT NULL) \
                             OR (i.kind = 'season' AND l.anime = 0 \
                                 AND i.season_number IS NOT NULL \
                                 AND parent.kind = 'show' AND parent.metadata_at IS NOT NULL) \
                             OR (i.kind = 'episode' AND l.anime = 0 \
                                 AND i.season_number IS NOT NULL AND parent.kind = 'season' \
                                 AND grandparent.kind = 'show' \
                                 AND grandparent.metadata_at IS NOT NULL)) \
                           AND (i.artwork_attempted_at IS NULL OR i.artwork_attempted_at <= $1) \
                           AND ($2 IS NULL OR i.library_id = $2) \
                         ORDER BY i.artwork_attempted_at, i.id LIMIT $3",
                        i = item_cols("i")
                    ),
                    params!(cutoff, library_id, limit),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn items_missing_genres(
        &self,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<Item>, StoreError> {
        items(
            self.client
                .query_consistent_map::<ItemRow, _>(
                    format!(
                        "SELECT {i} FROM items i \
                         JOIN libraries l ON l.id = i.library_id AND l.kind != 'home' \
                         WHERE i.kind IN ('movie','show') AND i.genres = '[]' \
                           AND i.metadata_at IS NOT NULL AND i.id > $1 \
                         ORDER BY i.id LIMIT $2",
                        i = item_cols("i")
                    ),
                    params!(after_id, limit.max(0)),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn update_item_fields(
        &self,
        item_id: i64,
        edit: &ItemEdit,
    ) -> Result<Option<Item>, StoreError> {
        let sort_title = edit.title.as_deref().map(sort_title_for);
        let tags = edit
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(database_error)?;
        let now = self.now()?;
        let sql = format!(
            "UPDATE items SET \
                 title = CASE WHEN $1 THEN $2 ELSE title END, \
                 sort_title = CASE WHEN $1 THEN $3 ELSE sort_title END, \
                 overview = CASE WHEN $4 THEN $5 ELSE overview END, \
                 recorded_at = CASE WHEN $6 THEN $7 ELSE recorded_at END, \
                 year = CASE WHEN $8 THEN $9 ELSE year END, \
                 tags = CASE WHEN $10 THEN $11 ELSE tags END, \
                 updated_at = $12 WHERE id = $13 RETURNING {ITEM_COLS}"
        );
        validate_sql(&sql)?;
        one_item(
            self.client
                .execute_returning_map::<_, ItemRow>(
                    sql,
                    params!(
                        edit.title.is_some(),
                        edit.title.as_deref(),
                        sort_title,
                        edit.overview.is_some(),
                        edit.overview.as_ref().and_then(|value| value.as_deref()),
                        edit.recorded_at.is_some(),
                        edit.recorded_at.as_ref().and_then(|value| value.as_deref()),
                        edit.year.is_some(),
                        edit.year.flatten(),
                        tags.is_some(),
                        tags,
                        now,
                        item_id
                    ),
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(database_error)?,
        )
    }

    async fn set_nfo_seeded(&self, item_id: i64) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE items SET nfo_seeded_at = $1 WHERE id = $2",
            params!(now, item_id),
        )
        .await?;
        Ok(())
    }

    async fn get_file_by_path(&self, path: &str) -> Result<Option<MediaFile>, StoreError> {
        one_file(
            self.client
                .query_consistent_map::<FileRow, _>(
                    format!("SELECT {FILE_COLS} FROM files WHERE path = $1"),
                    params!(path),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn upsert_file(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
    ) -> Result<i64, StoreError> {
        let audio = serde_json::to_string(&probe.audio_streams).map_err(database_error)?;
        let subtitles = serde_json::to_string(&probe.subtitle_streams).map_err(database_error)?;
        let now = self.now()?;
        let sql = "INSERT INTO files \
                   (item_id, path, size, mtime, duration_ms, container, video_codec, \
                    video_profile, width, height, bit_depth, hdr, bitrate, \
                    audio_streams, subtitle_streams, probe_json, hdr_format, scanned_at) \
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                           $14, $15, $16, $17, $18) \
                   ON CONFLICT(path) DO UPDATE SET \
                     item_id = excluded.item_id, size = excluded.size, mtime = excluded.mtime, \
                     duration_ms = excluded.duration_ms, container = excluded.container, \
                     video_codec = excluded.video_codec, video_profile = excluded.video_profile, \
                     width = excluded.width, height = excluded.height, bit_depth = excluded.bit_depth, \
                     hdr = excluded.hdr, bitrate = excluded.bitrate, \
                     audio_streams = excluded.audio_streams, \
                     subtitle_streams = excluded.subtitle_streams, \
                     probe_json = excluded.probe_json, hdr_format = excluded.hdr_format, \
                     scanned_at = excluded.scanned_at RETURNING id";
        validate_sql(sql)?;
        let row = self
            .client
            .execute_returning_map_one::<_, IdRow>(
                sql,
                params!(
                    item_id,
                    path,
                    size,
                    mtime,
                    probe.duration_ms,
                    probe.container.as_deref(),
                    probe.video_codec.as_deref(),
                    probe.video_profile.as_deref(),
                    probe.width,
                    probe.height,
                    probe.bit_depth,
                    probe.hdr.as_deref(),
                    probe.bitrate,
                    audio,
                    subtitles,
                    probe.raw_json.as_deref(),
                    probe.hdr_format.as_deref(),
                    now
                ),
            )
            .await
            .map_err(database_error)?;
        Ok(row.id)
    }

    async fn get_file(&self, id: i64) -> Result<Option<MediaFile>, StoreError> {
        one_file(
            self.client
                .query_consistent_map::<FileRow, _>(
                    format!("SELECT {FILE_COLS} FROM files WHERE id = $1"),
                    params!(id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn media_shape(&self) -> Result<MediaShape, StoreError> {
        let probed = scalar(
            &self.client,
            "SELECT COUNT(*) AS value FROM files WHERE video_codec IS NOT NULL",
        )
        .await?;
        let unprobed = scalar(
            &self.client,
            "SELECT COUNT(*) AS value FROM files WHERE video_codec IS NULL",
        )
        .await?;
        let hdr = pairs(
            &self.client,
            "SELECT COALESCE(NULLIF(hdr,''),'sdr') AS label, COUNT(*) AS count \
             FROM files WHERE video_codec IS NOT NULL GROUP BY 1",
        )
        .await?;
        let hdr_4k = pairs(
            &self.client,
            "SELECT COALESCE(NULLIF(hdr,''),'sdr') AS label, COUNT(*) AS count \
             FROM files WHERE video_codec IS NOT NULL AND height >= 1600 GROUP BY 1",
        )
        .await?;
        let codecs = pairs(
            &self.client,
            "SELECT LOWER(video_codec) AS label, COUNT(*) AS count \
             FROM files WHERE video_codec IS NOT NULL GROUP BY 1",
        )
        .await?;
        let over_segmented_floor = scalar(
            &self.client,
            "SELECT COUNT(*) AS value FROM files WHERE bitrate >= 40000000",
        )
        .await?;
        let max_bitrate = self
            .client
            .query_consistent_map::<OptionalScalarRow, _>(
                "SELECT MAX(bitrate) AS value FROM files",
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .and_then(|row| row.value);
        Ok(MediaShape {
            probed,
            unprobed,
            hdr,
            hdr_4k,
            codecs,
            over_segmented_floor,
            max_bitrate,
        })
    }

    async fn files_for_item(&self, item_id: i64) -> Result<Vec<MediaFile>, StoreError> {
        files(
            self.client
                .query_consistent_map::<FileRow, _>(
                    format!(
                        "SELECT {FILE_COLS} FROM files WHERE item_id = $1 \
                         ORDER BY height DESC, bitrate DESC, path"
                    ),
                    params!(item_id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn child_counts(&self, ids: &[i64]) -> Result<HashMap<i64, i64>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = id_list(ids);
        Ok(self
            .client
            .query_consistent_map::<ItemValueRow, _>(
                format!(
                    "SELECT parent_id AS item_id, COUNT(*) AS value FROM items \
                     WHERE parent_id IN ({list}) GROUP BY parent_id"
                ),
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.item_id, row.value))
            .collect())
    }

    async fn item_max_heights(&self, ids: &[i64]) -> Result<HashMap<i64, i64>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = id_list(ids);
        Ok(self
            .client
            .query_consistent_map::<ItemValueRow, _>(
                format!(
                    "SELECT item_id, MAX(height) AS value FROM files \
                     WHERE height IS NOT NULL AND item_id IN ({list}) GROUP BY item_id"
                ),
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.item_id, row.value))
            .collect())
    }

    async fn item_media_facts(&self, ids: &[i64]) -> Result<HashMap<i64, MediaFacts>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = id_list(ids);
        Ok(self
            .client
            .query_consistent_map::<FactsSqlRow, _>(
                format!(
                    "WITH ranked AS ( \
                         SELECT item_id, \
                                COUNT(*) OVER (PARTITION BY item_id) AS files, \
                                SUM(size) OVER (PARTITION BY item_id) AS bytes, \
                                ROW_NUMBER() OVER (PARTITION BY item_id \
                                    ORDER BY COALESCE(height, 0) DESC, \
                                             COALESCE(bitrate, 0) DESC, size DESC, id ASC) AS pick, \
                                container, video_codec, height, hdr, hdr_format, audio_streams \
                         FROM files WHERE item_id IN ({list}) \
                     ) \
                     SELECT item_id, files, bytes, container, video_codec, height, hdr, \
                            hdr_format, audio_streams FROM ranked WHERE pick = 1"
                ),
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| {
                let facts = FactsRow {
                    files: row.files,
                    bytes: row.bytes,
                    container: row.container,
                    video_codec: row.video_codec,
                    height: row.height,
                    hdr: row.hdr,
                    hdr_format: row.hdr_format,
                    audio: serde_json::from_str(&row.audio_streams).unwrap_or_default(),
                };
                (row.item_id, MediaFacts::from(facts))
            })
            .collect())
    }

    async fn set_file_audio_offset(&self, file_id: i64, offset_ms: i64) -> Result<(), StoreError> {
        self.execute(
            "UPDATE files SET audio_offset_ms = $1 WHERE id = $2",
            params!(offset_ms, file_id),
        )
        .await?;
        Ok(())
    }

    async fn get_file_probe_json(&self, file_id: i64) -> Result<Option<String>, StoreError> {
        Ok(self
            .client
            .query_consistent_map::<ProbeJsonRow, _>(
                "SELECT probe_json FROM files WHERE id = $1",
                params!(file_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .and_then(|row| row.probe_json))
    }

    async fn merge_file_probe_chapters(
        &self,
        file_id: i64,
        chapters_json: &str,
    ) -> Result<(), StoreError> {
        self.execute(
            "UPDATE files SET probe_json = json_set(probe_json, '$.chapters', json($1)) \
             WHERE id = $2 AND probe_json IS NOT NULL",
            params!(chapters_json, file_id),
        )
        .await?;
        Ok(())
    }

    async fn files_missing_probe(
        &self,
        library_id: Option<i64>,
    ) -> Result<Vec<MediaFile>, StoreError> {
        files(
            self.client
                .query_consistent_map::<FileRow, _>(
                    format!(
                        "SELECT {FILE_COLS} FROM files WHERE probe_json IS NULL \
                           AND ($1 IS NULL OR item_id IN \
                               (SELECT id FROM items WHERE library_id = $1)) \
                         ORDER BY scanned_at ASC, id ASC"
                    ),
                    params!(library_id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn library_file_paths(&self, library_id: i64) -> Result<Vec<(i64, PathBuf)>, StoreError> {
        Ok(self
            .client
            .query_consistent_map::<PathRow, _>(
                "SELECT f.id AS id, f.path AS path FROM files f \
                 JOIN items i ON i.id = f.item_id WHERE i.library_id = $1",
                params!(library_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.id, PathBuf::from(row.path)))
            .collect())
    }

    async fn ensure_library_root_fingerprint(
        &self,
        library_id: i64,
        fingerprint: &str,
        allow_establish: bool,
    ) -> Result<RootFingerprintStatus, StoreError> {
        let inserted = self
            .execute(
                "INSERT INTO library_roots (library_id, fingerprint) \
                 SELECT $1, $2 WHERE $3 \
                 ON CONFLICT(library_id) DO NOTHING",
                params!(library_id, fingerprint, allow_establish),
            )
            .await?;
        if inserted == 1 {
            return Ok(RootFingerprintStatus::Established);
        }
        let expected = self
            .client
            .query_consistent_map::<RootFingerprintRow, _>(
                "SELECT fingerprint FROM library_roots WHERE library_id = $1",
                params!(library_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(|row| row.fingerprint);
        let Some(expected) = expected else {
            return Ok(RootFingerprintStatus::Unestablished);
        };
        if expected == fingerprint {
            Ok(RootFingerprintStatus::Matched)
        } else {
            Ok(RootFingerprintStatus::Mismatch { expected })
        }
    }

    async fn reconcile_library(
        &self,
        library_id: i64,
        root_fingerprint: &str,
        gone_file_ids: &[i64],
        prune_limit: u64,
    ) -> Result<ReconcileOutcome, StoreError> {
        let expected = self
            .client
            .query_consistent_map::<RootFingerprintRow, _>(
                "SELECT fingerprint FROM library_roots WHERE library_id = $1",
                params!(library_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(|row| row.fingerprint);
        if expected.as_deref() != Some(root_fingerprint) {
            return Ok(ReconcileOutcome::RefusedRoot {
                expected: expected.unwrap_or_else(|| "<unregistered>".to_owned()),
            });
        }

        let list = if gone_file_ids.is_empty() {
            "NULL".to_owned()
        } else {
            id_list(gone_file_ids)
        };
        let requested = scalar_with(
            &self.client,
            format!(
                "SELECT COUNT(*) AS value FROM files f JOIN items i ON i.id = f.item_id \
                 WHERE i.library_id = $1 AND f.id IN ({list})"
            ),
            params!(library_id),
        )
        .await?
        .max(0) as u64;
        if requested > prune_limit {
            return Ok(ReconcileOutcome::RefusedPrune {
                requested,
                limit: prune_limit,
            });
        }

        let limit = i64::try_from(prune_limit).unwrap_or(i64::MAX);
        let statements: Vec<(String, hiqlite::Params)> = vec![
            (
                format!(
                    "INSERT INTO scan_reconcile_guards (library_id) \
                     SELECT $1 WHERE EXISTS (SELECT 1 FROM library_roots \
                         WHERE library_id = $1 AND fingerprint = $2) \
                     AND (SELECT COUNT(*) FROM files f JOIN items i ON i.id = f.item_id \
                          WHERE i.library_id = $1 AND f.id IN ({list})) <= $3 \
                     ON CONFLICT(library_id) DO NOTHING"
                ),
                params!(library_id, root_fingerprint, limit),
            ),
            (
                format!(
                    "DELETE FROM files WHERE id IN ({list}) \
                     AND item_id IN (SELECT id FROM items WHERE library_id = $1) \
                     AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)"
                ),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1 \
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1) \
                 AND kind IN ('movie','episode','video','photo') \
                 AND id NOT IN (SELECT item_id FROM files)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1 \
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1) \
                 AND kind = 'season' AND id NOT IN (SELECT parent_id FROM items \
                     WHERE kind = 'episode' AND parent_id IS NOT NULL)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1 \
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1) \
                 AND kind = 'show' AND id NOT IN (SELECT parent_id FROM items \
                     WHERE kind = 'season' AND parent_id IS NOT NULL)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "WITH RECURSIVE descendants(root_id, id, kind) AS ( \
                     SELECT root.id, child.id, child.kind FROM items root \
                     LEFT JOIN items child ON child.parent_id = root.id \
                     WHERE root.library_id = $1 AND root.kind = 'folder' \
                     UNION SELECT descendants.root_id, child.id, child.kind \
                     FROM descendants JOIN items child ON child.parent_id = descendants.id \
                 ) INSERT INTO scan_reconcile_items (library_id, item_id) \
                   SELECT $1, items.id FROM items \
                   WHERE items.library_id = $1 AND items.kind = 'folder' \
                     AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1) \
                     AND NOT EXISTS (SELECT 1 FROM descendants \
                                     WHERE root_id = items.id AND kind != 'folder') \
                   ON CONFLICT(library_id, item_id) DO NOTHING"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1 AND id IN \
                    (SELECT item_id FROM scan_reconcile_items WHERE library_id = $1)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM scan_reconcile_items WHERE library_id = $1".to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM scan_reconcile_guards WHERE library_id = $1".to_owned(),
                params!(library_id),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let results = self
            .client
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.first().copied() != Some(1) {
            let expected = self
                .client
                .query_consistent_map::<RootFingerprintRow, _>(
                    "SELECT fingerprint FROM library_roots WHERE library_id = $1",
                    params!(library_id),
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .next()
                .map(|row| row.fingerprint)
                .unwrap_or_else(|| "<unregistered>".to_owned());
            return Ok(ReconcileOutcome::RefusedRoot { expected });
        }
        Ok(ReconcileOutcome::Applied {
            deleted_files: results[1] as u64,
            pruned_items: results[2..=5].iter().map(|rows| *rows as u64).sum(),
        })
    }

    async fn reset_library_root_fingerprint(&self, library_id: i64) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "DELETE FROM library_roots WHERE library_id = $1",
                params!(library_id),
            )
            .await?
            == 1)
    }

    async fn rebuild_search_index(&self) -> Result<u64, StoreError> {
        let statements = [
            ("DELETE FROM items_fts", params!()),
            (
                "INSERT INTO items_fts(rowid, title, overview, tags) \
                 SELECT id, title, overview, tags FROM items",
                params!(),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let results = self
            .client
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        Ok(results.get(1).copied().unwrap_or(0) as u64)
    }

    async fn delete_files(&self, ids: &[i64]) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let sql = "DELETE FROM files WHERE id = $1";
        validate_sql(sql)?;
        let results = self
            .client
            .txn(ids.iter().map(|id| (sql, params!(*id))))
            .await
            .map_err(database_error)?;
        results.into_iter().try_fold(0_u64, |deleted, result| {
            result
                .map(|rows| deleted + rows as u64)
                .map_err(database_error)
        })
    }

    async fn prune_empty_items(&self, library_id: i64) -> Result<u64, StoreError> {
        let statements = [
            "DELETE FROM scan_reconcile_items WHERE library_id = $1",
            "DELETE FROM items WHERE library_id = $1 \
             AND kind IN ('movie','episode','video','photo') \
             AND id NOT IN (SELECT item_id FROM files)",
            "DELETE FROM items WHERE library_id = $1 AND kind = 'season' \
             AND id NOT IN (SELECT parent_id FROM items \
                            WHERE kind = 'episode' AND parent_id IS NOT NULL)",
            "DELETE FROM items WHERE library_id = $1 AND kind = 'show' \
             AND id NOT IN (SELECT parent_id FROM items \
                            WHERE kind = 'season' AND parent_id IS NOT NULL)",
            "WITH RECURSIVE descendants(root_id, id, kind) AS ( \
                 SELECT root.id, child.id, child.kind FROM items root \
                 LEFT JOIN items child ON child.parent_id = root.id \
                 WHERE root.library_id = $1 AND root.kind = 'folder' \
                 UNION \
                 SELECT descendants.root_id, child.id, child.kind \
                 FROM descendants JOIN items child ON child.parent_id = descendants.id \
             ) \
             INSERT INTO scan_reconcile_items (library_id, item_id) \
               SELECT $1, items.id FROM items \
               WHERE items.library_id = $1 AND items.kind = 'folder' \
                 AND NOT EXISTS (SELECT 1 FROM descendants \
                                 WHERE root_id = items.id AND kind != 'folder') \
               ON CONFLICT(library_id, item_id) DO NOTHING",
            "DELETE FROM items WHERE library_id = $1 AND id IN \
                (SELECT item_id FROM scan_reconcile_items WHERE library_id = $1)",
            "DELETE FROM scan_reconcile_items WHERE library_id = $1",
        ];
        for sql in statements {
            validate_sql(sql)?;
        }
        let results = self
            .client
            .txn(statements.map(|sql| (sql, params!(library_id))))
            .await
            .map_err(database_error)?;
        let results = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        // The staging INSERT count is the exact number of folders selected;
        // CTE DELETE row counts vary across SQLite wrappers.
        Ok(results[1..=4].iter().map(|rows| *rows as u64).sum())
    }
}

const PLAYABLE_KINDS: &str = "'movie','episode','video'";

#[async_trait]
impl WatchStore for HiqliteAuthStore {
    async fn watch_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<WatchState>, StoreError> {
        Ok(self
            .client
            .query_consistent_map::<WatchRow, _>(
                "SELECT position_ms, duration_ms, watched, updated_at \
                 FROM watch_state WHERE user_id = $1 AND item_id = $2",
                params!(user_id, item_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into))
    }

    async fn watch_map(
        &self,
        user_id: i64,
        item_ids: &[i64],
    ) -> Result<Vec<(i64, WatchState)>, StoreError> {
        if item_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(item_ids).map_err(database_error)?;
        Ok(self
            .client
            .query_consistent_map::<WatchMapRow, _>(
                "SELECT w.item_id, w.position_ms, w.duration_ms, w.watched, w.updated_at \
                 FROM watch_state w JOIN json_each($1) j ON j.value = w.item_id \
                 WHERE w.user_id = $2",
                params!(ids_json, user_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.item_id, row.state.into()))
            .collect())
    }

    async fn put_progress_at(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
        recorded_at: Option<i64>,
    ) -> Result<WatchState, StoreError> {
        let now = self.now()?;
        let at = recorded_at.unwrap_or(now).clamp(0, now);
        let sql = "WITH input(duration_ms) AS ( \
                       SELECT COALESCE( \
                           (SELECT MAX(duration_ms) FROM files \
                            WHERE item_id = $1 AND duration_ms > 0), \
                           CASE WHEN $2 > 0 THEN $2 END) \
                   ), normalized(position_ms, duration_ms) AS ( \
                       SELECT CASE WHEN duration_ms IS NULL THEN MAX($3, 0) \
                                   ELSE MIN(MAX($3, 0), duration_ms) END, duration_ms \
                       FROM input \
                   ) \
                   INSERT INTO watch_state \
                       (user_id, item_id, position_ms, duration_ms, watched, updated_at) \
                   SELECT $4, $1, position_ms, duration_ms, \
                          CASE WHEN duration_ms IS NOT NULL \
                                    AND (position_ms * 1.0 / duration_ms) >= 0.95 \
                               THEN 1 ELSE 0 END, $5 \
                   FROM normalized WHERE true \
                   ON CONFLICT(user_id, item_id) DO UPDATE SET \
                       position_ms = excluded.position_ms, \
                       duration_ms = COALESCE(excluded.duration_ms, watch_state.duration_ms), \
                       watched = watch_state.watched OR excluded.watched, \
                       updated_at = excluded.updated_at \
                   WHERE $6 = 1 OR excluded.updated_at >= watch_state.updated_at \
                   RETURNING position_ms, duration_ms, watched, updated_at";
        validate_sql(sql)?;
        let returned = self
            .client
            .execute_returning_map::<_, WatchRow>(
                sql,
                params!(
                    item_id,
                    duration_ms,
                    position_ms,
                    user_id,
                    at,
                    recorded_at.is_none()
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
        self.watch_state(user_id, item_id).await?.ok_or_else(|| {
            StoreError::Database("stale progress replay found no current watch row".to_owned())
        })
    }

    async fn set_watched(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        if watched {
            self.execute(
                "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at) \
                 VALUES ($1, $2, 0, 1, $3) \
                 ON CONFLICT(user_id, item_id) DO UPDATE SET watched = 1, updated_at = $3",
                params!(user_id, item_id, now),
            )
            .await?;
        } else {
            self.execute(
                "INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at) \
                 VALUES ($1, $2, 0, 0, $3) \
                 ON CONFLICT(user_id, item_id) DO UPDATE SET \
                     watched = 0, position_ms = 0, updated_at = $3",
                params!(user_id, item_id, now),
            )
            .await?;
        }
        Ok(())
    }

    async fn set_watched_tree(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<Vec<i64>, StoreError> {
        let now = self.now()?;
        let sql = if watched {
            format!(
                "WITH RECURSIVE tree(id) AS ( \
                     SELECT id FROM items WHERE id = $1 \
                     UNION SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id \
                 ) \
                 INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at) \
                 SELECT $2, i.id, 0, 1, $3 FROM tree t JOIN items i ON i.id = t.id \
                 WHERE i.kind IN ({PLAYABLE_KINDS}) \
                 ON CONFLICT(user_id, item_id) DO UPDATE SET watched = 1, updated_at = $3 \
                 WHERE watch_state.watched = 0 RETURNING item_id AS id"
            )
        } else {
            format!(
                "WITH RECURSIVE tree(id) AS ( \
                     SELECT id FROM items WHERE id = $1 \
                     UNION SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id \
                 ) \
                 INSERT INTO watch_state (user_id, item_id, position_ms, watched, updated_at) \
                 SELECT $2, i.id, 0, 0, $3 FROM tree t JOIN items i ON i.id = t.id \
                 WHERE i.kind IN ({PLAYABLE_KINDS}) \
                 ON CONFLICT(user_id, item_id) DO UPDATE SET \
                     watched = 0, position_ms = 0, updated_at = $3 \
                 WHERE watch_state.watched = 1 OR watch_state.position_ms <> 0 \
                 RETURNING item_id AS id"
            )
        };
        validate_sql(&sql)?;
        let mut changed = self
            .client
            .execute_returning_map::<_, IdRow>(sql, params!(item_id, user_id, now))
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        changed.sort_unstable();
        Ok(changed)
    }

    async fn watch_rollup(&self, user_id: i64, item_id: i64) -> Result<WatchRollup, StoreError> {
        let rows = self
            .client
            .query_consistent_map::<RollupRow, _>(
                format!(
                    "WITH RECURSIVE tree(id) AS ( \
                         SELECT id FROM items WHERE id = $1 \
                         UNION SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id \
                     ) \
                     SELECT $1 AS root, COUNT(*) AS leaves, \
                            COALESCE(SUM(w.watched), 0) AS watched \
                     FROM tree t JOIN items i ON i.id = t.id \
                     LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = $2 \
                     WHERE i.kind IN ({PLAYABLE_KINDS})"
                ),
                params!(item_id, user_id),
            )
            .await
            .map_err(database_error)?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Database("watch rollup returned no row".to_owned()))?;
        Ok(WatchRollup {
            leaves: row.leaves,
            watched: row.watched,
        })
    }

    async fn watch_rollups(
        &self,
        user_id: i64,
        ids: &[i64],
    ) -> Result<HashMap<i64, WatchRollup>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = id_list(ids);
        let rows = self
            .client
            .query_consistent_map::<RollupRow, _>(
                format!(
                    "WITH RECURSIVE tree(root, id) AS ( \
                         SELECT id, id FROM items WHERE id IN ({list}) \
                         UNION SELECT t.root, i.id FROM items i JOIN tree t ON i.parent_id = t.id \
                     ) \
                     SELECT t.root AS root, COUNT(*) AS leaves, \
                            COALESCE(SUM(w.watched), 0) AS watched \
                     FROM tree t JOIN items i ON i.id = t.id \
                     LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = $1 \
                     WHERE i.kind IN ({PLAYABLE_KINDS}) GROUP BY t.root"
                ),
                params!(user_id),
            )
            .await
            .map_err(database_error)?;
        let mut rollups: HashMap<_, _> = ids
            .iter()
            .copied()
            .map(|id| (id, WatchRollup::default()))
            .collect();
        rollups.extend(rows.into_iter().map(|row| {
            (
                row.root,
                WatchRollup {
                    leaves: row.leaves,
                    watched: row.watched,
                },
            )
        }));
        Ok(rollups)
    }

    async fn continue_watching(
        &self,
        user_id: i64,
        limit: i64,
    ) -> Result<Vec<InProgressItem>, StoreError> {
        let sql = format!(
            "SELECT {i}, show.title AS rail_show_title, \
                    season.poster_path AS rail_season_poster, \
                    w.position_ms AS watch_position_ms, \
                    w.duration_ms AS watch_duration_ms, \
                    w.watched AS watch_watched, w.updated_at AS watch_updated_at \
             FROM watch_state w JOIN items i ON i.id = w.item_id \
             LEFT JOIN items season ON season.id = i.parent_id AND i.kind = 'episode' \
             LEFT JOIN items show ON show.id = season.parent_id \
             WHERE w.user_id = $1 AND w.watched = 0 AND w.position_ms > 0 \
               AND i.kind IN ('movie','episode','video') \
             ORDER BY w.updated_at DESC LIMIT $2",
            i = item_cols("i")
        );
        self.client
            .query_consistent_map::<InProgressRow, _>(sql, params!(user_id, limit))
            .await
            .map_err(database_error)?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn next_up(&self, user_id: i64, limit: i64) -> Result<Vec<RecentItem>, StoreError> {
        let sql = format!(
            "SELECT {e}, show.title AS rail_show_title, \
                    season.poster_path AS rail_season_poster, \
                    MIN(season.season_number*100000 + e.episode_number) AS ord \
             FROM items e JOIN items season ON season.id = e.parent_id \
             JOIN items show ON show.id = season.parent_id \
             WHERE e.kind = 'episode' \
               AND e.id NOT IN (SELECT item_id FROM watch_state \
                                WHERE user_id = $1 AND (watched = 1 OR position_ms > 0)) \
               AND (season.season_number*100000 + e.episode_number) > ( \
                   SELECT COALESCE(MAX(se.season_number*100000 + ep.episode_number), -1) \
                   FROM watch_state w JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode' \
                   JOIN items se ON se.id = ep.parent_id \
                   WHERE w.user_id = $1 AND w.watched = 1 AND se.parent_id = show.id) \
               AND show.id IN (SELECT sh.id FROM watch_state w \
                   JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode' \
                   JOIN items se ON se.id = ep.parent_id JOIN items sh ON sh.id = se.parent_id \
                   WHERE w.user_id = $1 AND w.watched = 1) \
               AND show.id NOT IN (SELECT sh.id FROM watch_state w \
                   JOIN items ep ON ep.id = w.item_id AND ep.kind = 'episode' \
                   JOIN items se ON se.id = ep.parent_id JOIN items sh ON sh.id = se.parent_id \
                   WHERE w.user_id = $1 AND w.watched = 0 AND w.position_ms > 0) \
             GROUP BY show.id ORDER BY show.sort_title LIMIT $2",
            e = item_cols("e")
        );
        recent_items(
            self.client
                .query_consistent_map::<RecentItemRow, _>(sql, params!(user_id, limit))
                .await
                .map_err(database_error)?,
        )
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
        let at = updated_at.clamp(0, self.now()?);
        self.execute(
            "INSERT INTO watch_state \
                 (user_id, item_id, position_ms, duration_ms, watched, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT(user_id, item_id) DO UPDATE SET \
                 position_ms = excluded.position_ms, \
                 duration_ms = COALESCE(excluded.duration_ms, watch_state.duration_ms), \
                 watched = excluded.watched, updated_at = excluded.updated_at",
            params!(user_id, item_id, position_ms, duration_ms, watched, at),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_expression_quotes_tokens_and_prefixes_only_the_last() {
        assert_eq!(
            fts_query("The Matrix"),
            Some("\"the\" \"matrix\"*".to_owned())
        );
        assert_eq!(fts_query("..."), None);
    }
}
