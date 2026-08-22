//! Replicated libraries, media catalogue, watch state, and derived local FTS.
//!
//! The authoritative rows below travel through Raft. `items_fts` is an
//! contentless derived index rebuilt from `items`; each voter answers search from
//! its own copy so losing and rebuilding that index cannot alter cluster truth.

use std::path::PathBuf;

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::hiqlite::{database_error, timeout_store, validate_sql, HiqliteAuthStore, TimedClient};
use super::LibraryStore;
use crate::domain::{Library, LibraryKind, NewLibrary};
use crate::error::StoreError;

const CATALOG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS libraries (
    id                    INTEGER PRIMARY KEY,
    name                  TEXT NOT NULL UNIQUE,
    kind                  TEXT NOT NULL,
    paths                 TEXT NOT NULL,
    anime                 INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    scan_interval_mins    INTEGER NOT NULL DEFAULT 0,
    refresh_interval_mins INTEGER NOT NULL DEFAULT 0,
    last_scan_at          INTEGER,
    last_refresh_at       INTEGER
) STRICT;

CREATE TABLE IF NOT EXISTS items (
    id                   INTEGER PRIMARY KEY,
    library_id           INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    kind                 TEXT NOT NULL,
    parent_id            INTEGER REFERENCES items(id) ON DELETE CASCADE,
    title                TEXT NOT NULL,
    sort_title           TEXT NOT NULL,
    year                 INTEGER,
    overview             TEXT,
    tmdb_id              INTEGER,
    imdb_id              TEXT,
    season_number        INTEGER,
    episode_number       INTEGER,
    air_date             TEXT,
    runtime_ms           INTEGER,
    poster_path          TEXT,
    backdrop_path        TEXT,
    added_at             INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    recorded_at          TEXT,
    tags                 TEXT NOT NULL DEFAULT '[]',
    nfo_seeded_at        INTEGER,
    metadata_at          INTEGER,
    artwork_attempted_at INTEGER,
    artwork_error        TEXT,
    genres               TEXT NOT NULL DEFAULT '[]',
    author               TEXT,
    book_work_id         TEXT,
    book_edition_id      TEXT,
    book_metadata_source TEXT CHECK (book_metadata_source IN ('epub', 'curator'))
) STRICT;
CREATE INDEX IF NOT EXISTS idx_items_library_kind ON items(library_id, kind);
CREATE INDEX IF NOT EXISTS idx_items_parent ON items(parent_id);
CREATE INDEX IF NOT EXISTS idx_items_added ON items(added_at DESC);
CREATE INDEX IF NOT EXISTS idx_items_missing_artwork ON items(artwork_attempted_at)
    WHERE poster_path IS NULL;
CREATE INDEX IF NOT EXISTS idx_items_book_work ON items(book_work_id)
    WHERE book_work_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS files (
    id               INTEGER PRIMARY KEY,
    item_id          INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    path             TEXT NOT NULL UNIQUE,
    size             INTEGER NOT NULL,
    mtime            INTEGER NOT NULL,
    duration_ms      INTEGER,
    container        TEXT,
    video_codec      TEXT,
    video_profile    TEXT,
    width            INTEGER,
    height           INTEGER,
    bit_depth        INTEGER,
    hdr              TEXT,
    bitrate          INTEGER,
    audio_streams    TEXT NOT NULL DEFAULT '[]',
    subtitle_streams TEXT NOT NULL DEFAULT '[]',
    probe_json       TEXT,
    scanned_at       INTEGER NOT NULL,
    hdr_format       TEXT,
    audio_offset_ms  INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX IF NOT EXISTS idx_files_item ON files(item_id);

CREATE TABLE IF NOT EXISTS watch_state (
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    watched     INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_id)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_watch_updated ON watch_state(user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS library_roots (
    library_id  INTEGER PRIMARY KEY REFERENCES libraries(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS library_roots_paths_au AFTER UPDATE OF paths ON libraries
WHEN old.paths <> new.paths BEGIN
    DELETE FROM library_roots WHERE library_id = new.id;
END;

CREATE TABLE IF NOT EXISTS scan_reconcile_guards (
    library_id INTEGER PRIMARY KEY REFERENCES libraries(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS scan_reconcile_items (
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    item_id    INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    PRIMARY KEY (library_id, item_id)
) STRICT;

CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
    title, overview, tags, content='', contentless_delete=1
);
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts_vocab USING fts5vocab(items_fts, 'instance');
CREATE TRIGGER IF NOT EXISTS items_fts_ai AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, title, overview, tags)
    VALUES (new.id, new.title, new.overview, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS items_fts_ad AFTER DELETE ON items BEGIN
    DELETE FROM items_fts WHERE rowid = old.id;
END;
CREATE TRIGGER IF NOT EXISTS items_fts_au AFTER UPDATE OF title, overview, tags ON items BEGIN
    DELETE FROM items_fts WHERE rowid = old.id;
    INSERT INTO items_fts(rowid, title, overview, tags)
    VALUES (new.id, new.title, new.overview, new.tags);
END;
"#;

// Kept as individual statements because replicated migrations execute schema
// changes and their cluster_meta bump in one Raft transaction. Fresh bootstrap
// uses these exact strings too, so the two paths cannot drift.
pub(super) const READING_STATE_TABLE_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS reading_state (
    user_id            INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id            INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    file_id            INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    file_size          INTEGER NOT NULL,
    file_mtime         INTEGER NOT NULL,
    locator_json       TEXT NOT NULL,
    progression_millis INTEGER NOT NULL CHECK (progression_millis BETWEEN 0 AND 1000000),
    completed          INTEGER NOT NULL CHECK (completed IN (0, 1)),
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_id, file_id)
) STRICT"#;

pub(super) const READING_STATE_INDEX_SCHEMA: &str = r#"CREATE INDEX IF NOT EXISTS idx_reading_updated
    ON reading_state(user_id, updated_at DESC)"#;

pub(super) const BOOK_AUTHOR_SCHEMA: &str = "ALTER TABLE items ADD COLUMN author TEXT";
pub(super) const BOOK_WORK_SCHEMA: &str = "ALTER TABLE items ADD COLUMN book_work_id TEXT";
pub(super) const BOOK_EDITION_SCHEMA: &str = "ALTER TABLE items ADD COLUMN book_edition_id TEXT";
pub(super) const BOOK_SOURCE_SCHEMA: &str =
    "ALTER TABLE items ADD COLUMN book_metadata_source TEXT \
    CHECK (book_metadata_source IN ('epub', 'curator'))";
pub(super) const BOOK_WORK_INDEX_SCHEMA: &str = "CREATE INDEX IF NOT EXISTS idx_items_book_work \
    ON items(book_work_id) WHERE book_work_id IS NOT NULL";

pub(super) async fn install_schema(client: &hiqlite::Client) -> Result<(), StoreError> {
    validate_sql(CATALOG_SCHEMA)?;
    for result in timeout_store(client.batch(CATALOG_SCHEMA)).await? {
        result.map_err(database_error)?;
    }
    for sql in [READING_STATE_TABLE_SCHEMA, READING_STATE_INDEX_SCHEMA] {
        validate_sql(sql)?;
        timeout_store(client.execute(sql, params!())).await?;
    }
    Ok(())
}

struct JsonValueRow {
    value: String,
}

async fn rows(client: &TimedClient, sql: &'static str) -> Result<Vec<String>, StoreError> {
    Ok(client
        .query_map::<JsonValueRow, _>(sql, params!())
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|row| row.value)
        .collect())
}

impl From<&mut Row<'_>> for JsonValueRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

#[derive(Serialize)]
struct CatalogDump {
    libraries: Vec<String>,
    items: Vec<String>,
    files: Vec<String>,
    watch_state: Vec<String>,
    reading_state: Vec<String>,
    library_roots: Vec<String>,
    scan_reconcile_guards: Vec<String>,
    scan_reconcile_items: Vec<String>,
    fts_terms: Vec<String>,
    fts_schema: Vec<String>,
}

#[derive(Serialize)]
struct CatalogTruthDump {
    libraries: Vec<String>,
    items: Vec<String>,
    files: Vec<String>,
    watch_state: Vec<String>,
    reading_state: Vec<String>,
    library_roots: Vec<String>,
    scan_reconcile_guards: Vec<String>,
    scan_reconcile_items: Vec<String>,
}

async fn authoritative_dump(client: &TimedClient) -> Result<CatalogTruthDump, StoreError> {
    Ok(CatalogTruthDump {
        libraries: rows(
            client,
            "SELECT json_array(id, name, kind, paths, anime, created_at, \
                    scan_interval_mins, refresh_interval_mins, last_scan_at, last_refresh_at) \
             AS value FROM libraries ORDER BY id",
        )
        .await?,
        items: rows(
            client,
            "SELECT json_array(id, library_id, kind, parent_id, title, sort_title, year, \
                    overview, tmdb_id, imdb_id, season_number, episode_number, air_date, \
                    runtime_ms, poster_path, backdrop_path, added_at, updated_at, recorded_at, \
                    tags, nfo_seeded_at, metadata_at, artwork_attempted_at, artwork_error, genres) \
             AS value FROM items ORDER BY id",
        )
        .await?,
        files: rows(
            client,
            "SELECT json_array(id, item_id, path, size, mtime, duration_ms, container, \
                    video_codec, video_profile, width, height, bit_depth, hdr, bitrate, \
                    audio_streams, subtitle_streams, probe_json, scanned_at, hdr_format, \
                    audio_offset_ms) AS value FROM files ORDER BY id",
        )
        .await?,
        watch_state: rows(
            client,
            "SELECT json_array(user_id, item_id, position_ms, duration_ms, watched, updated_at) \
             AS value FROM watch_state ORDER BY user_id, item_id",
        )
        .await?,
        reading_state: rows(
            client,
            "SELECT json_array(user_id, item_id, file_id, file_size, file_mtime, \
                    locator_json, progression_millis, completed, updated_at) \
             AS value FROM reading_state ORDER BY user_id, item_id, file_id",
        )
        .await?,
        library_roots: rows(
            client,
            "SELECT json_array(library_id, fingerprint) AS value \
             FROM library_roots ORDER BY library_id",
        )
        .await?,
        scan_reconcile_guards: rows(
            client,
            "SELECT json_array(library_id) AS value \
             FROM scan_reconcile_guards ORDER BY library_id",
        )
        .await?,
        scan_reconcile_items: rows(
            client,
            "SELECT json_array(library_id, item_id) AS value \
             FROM scan_reconcile_items ORDER BY library_id, item_id",
        )
        .await?,
    })
}

pub(super) async fn local_catalog_truth_digest(client: &TimedClient) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(&authoritative_dump(client).await?).map_err(database_error)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(super) async fn local_catalog_digest(client: &TimedClient) -> Result<String, StoreError> {
    let truth = authoritative_dump(client).await?;
    let dump = CatalogDump {
        libraries: truth.libraries,
        items: truth.items,
        files: truth.files,
        watch_state: truth.watch_state,
        reading_state: truth.reading_state,
        library_roots: truth.library_roots,
        scan_reconcile_guards: truth.scan_reconcile_guards,
        scan_reconcile_items: truth.scan_reconcile_items,
        fts_terms: rows(
            client,
            "SELECT json_array(term, doc, col, offset) AS value \
             FROM items_fts_vocab ORDER BY term, doc, col, offset",
        )
        .await?,
        fts_schema: rows(
            client,
            "SELECT json_array(name, type, sql) AS value FROM sqlite_master \
             WHERE name IN ('items_fts','items_fts_vocab','items_fts_ai', \
                            'items_fts_ad','items_fts_au','library_roots_paths_au') \
             ORDER BY name",
        )
        .await?,
    };
    let bytes = serde_json::to_vec(&dump).map_err(database_error)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod query_helper_tests {
    use super::*;
    use crate::store::hiqlite::disconnected_test_client;

    #[tokio::test]
    async fn rows_helper_rejects_misordered_placeholders() {
        let error = rows(
            &disconnected_test_client(),
            "SELECT $2 AS value WHERE $1 = 1",
        )
        .await
        .expect_err("catalogue rows helper must validate SQL");
        assert!(error.to_string().contains("expected $1, found $2"));
    }
}

const LIB_COLS: &str = "id, name, kind, paths, anime, created_at, scan_interval_mins, \
     refresh_interval_mins, last_scan_at, last_refresh_at";

struct LibraryRow {
    id: i64,
    name: String,
    kind: String,
    paths: String,
    anime: i64,
    created_at: i64,
    scan_interval_mins: i64,
    refresh_interval_mins: i64,
    last_scan_at: Option<i64>,
    last_refresh_at: Option<i64>,
}

impl From<&mut Row<'_>> for LibraryRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            name: row.get("name"),
            kind: row.get("kind"),
            paths: row.get("paths"),
            anime: row.get("anime"),
            created_at: row.get("created_at"),
            scan_interval_mins: row.get("scan_interval_mins"),
            refresh_interval_mins: row.get("refresh_interval_mins"),
            last_scan_at: row.get("last_scan_at"),
            last_refresh_at: row.get("last_refresh_at"),
        }
    }
}

impl TryFrom<LibraryRow> for Library {
    type Error = StoreError;

    fn try_from(row: LibraryRow) -> Result<Self, Self::Error> {
        let kind = LibraryKind::parse(&row.kind)
            .ok_or_else(|| StoreError::Database(format!("unknown library kind `{}`", row.kind)))?;
        let paths: Vec<PathBuf> = serde_json::from_str(&row.paths)
            .map_err(|error| StoreError::Database(format!("library paths: {error}")))?;
        Ok(Self {
            id: row.id,
            name: row.name,
            kind,
            paths,
            anime: row.anime != 0,
            created_at: row.created_at,
            scan_interval_mins: row.scan_interval_mins,
            refresh_interval_mins: row.refresh_interval_mins,
            last_scan_at: row.last_scan_at,
            last_refresh_at: row.last_refresh_at,
        })
    }
}

fn paths_json(paths: &[PathBuf]) -> Result<String, StoreError> {
    serde_json::to_string(paths).map_err(|error| StoreError::Database(error.to_string()))
}

fn one_library(rows: Vec<LibraryRow>) -> Result<Option<Library>, StoreError> {
    rows.into_iter().next().map(TryInto::try_into).transpose()
}

fn one_returning_library(
    rows: Vec<Result<LibraryRow, hiqlite::Error>>,
) -> Result<Option<Library>, StoreError> {
    one_library(
        rows.into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?,
    )
}

#[async_trait]
impl LibraryStore for HiqliteAuthStore {
    async fn create_library(&self, library: &NewLibrary) -> Result<Library, StoreError> {
        let now = self.now()?;
        let paths = paths_json(&library.paths)?;
        let sql = format!(
            "INSERT INTO libraries (name, kind, paths, anime, created_at) \
             VALUES ($1, $2, $3, $4, $5) RETURNING {LIB_COLS}"
        );
        validate_sql(&sql)?;
        let row = self
            .client()
            .execute_returning_map_one::<_, LibraryRow>(
                sql,
                params!(
                    library.name.as_str(),
                    library.kind.as_str(),
                    paths,
                    library.anime,
                    now
                ),
            )
            .await
            .map_err(database_error)?;
        row.try_into()
    }

    async fn update_library(
        &self,
        id: i64,
        library: &NewLibrary,
    ) -> Result<Option<Library>, StoreError> {
        let paths = paths_json(&library.paths)?;
        let sql = format!(
            "UPDATE libraries SET name = $1, kind = $2, paths = $3, anime = $4 \
             WHERE id = $5 RETURNING {LIB_COLS}"
        );
        validate_sql(&sql)?;
        one_returning_library(
            self.client()
                .execute_returning_map::<_, LibraryRow>(
                    sql,
                    params!(
                        library.name.as_str(),
                        library.kind.as_str(),
                        paths,
                        library.anime,
                        id
                    ),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn set_library_schedule(
        &self,
        id: i64,
        scan_interval_mins: i64,
        refresh_interval_mins: i64,
    ) -> Result<Option<Library>, StoreError> {
        let sql = format!(
            "UPDATE libraries SET scan_interval_mins = $1, refresh_interval_mins = $2 \
             WHERE id = $3 RETURNING {LIB_COLS}"
        );
        validate_sql(&sql)?;
        one_returning_library(
            self.client()
                .execute_returning_map::<_, LibraryRow>(
                    sql,
                    params!(scan_interval_mins.max(0), refresh_interval_mins.max(0), id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn mark_library_scanned(&self, id: i64, refreshed: bool) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE libraries SET last_scan_at = $1, \
             last_refresh_at = CASE WHEN $2 THEN $1 ELSE last_refresh_at END \
             WHERE id = $3",
            params!(now, refreshed, id),
        )
        .await?;
        Ok(())
    }

    async fn delete_library(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .execute("DELETE FROM libraries WHERE id = $1", params!(id))
            .await?
            > 0)
    }

    async fn get_library(&self, id: i64) -> Result<Option<Library>, StoreError> {
        one_library(
            self.client()
                .query_consistent_map::<LibraryRow, _>(
                    format!("SELECT {LIB_COLS} FROM libraries WHERE id = $1"),
                    params!(id),
                )
                .await
                .map_err(database_error)?,
        )
    }

    async fn list_libraries(&self) -> Result<Vec<Library>, StoreError> {
        self.client()
            .query_consistent_map::<LibraryRow, _>(
                format!("SELECT {LIB_COLS} FROM libraries ORDER BY name"),
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_catalog_schema_binds_every_clock_value() {
        validate_sql(CATALOG_SCHEMA).expect("schema contains no local clock or RNG calls");
    }
}
